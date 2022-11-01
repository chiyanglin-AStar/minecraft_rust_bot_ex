use crate::{bot, HandleFn};
use azalea_client::{Account, Client, Event, JoinError, Plugin, Plugins};
use azalea_protocol::{
    resolver::{self, ResolverError},
    ServerAddress,
};
use futures::{
    future::{select_all, try_join_all},
    FutureExt,
};
use parking_lot::Mutex;
use std::{future::Future, sync::Arc};
use thiserror::Error;
use tokio::sync::mpsc::UnboundedReceiver;

/// A swarm is a way to conveniently control many bots at once, while also
/// being able to control bots at an individual level when desired.
#[derive(Clone)]
pub struct Swarm {
    bots: Arc<Mutex<Vec<Client>>>,
    receivers: Arc<Mutex<Vec<UnboundedReceiver<Event>>>>,
}

/// An event about something that doesn't have to do with a single bot.
#[derive(Clone, Debug)]
pub enum SwarmEvent {
    /// All the bots in the swarm have successfully joined the server.
    Login,
}

/// The options that are passed to [`azalea::start_swarm`].
///
/// [`azalea::start`]: crate::start_swarm
pub struct SwarmOptions<S, SS, A, Fut>
where
    A: TryInto<ServerAddress>,
    Fut: Future<Output = Result<(), anyhow::Error>>,
{
    /// The address of the server that we're connecting to. This can be a
    /// `&str`, [`ServerAddress`], or anything that implements
    /// `TryInto<ServerAddress>`.
    ///
    /// [`ServerAddress`]: azalea_protocol::ServerAddress
    pub address: A,
    /// The accounts that are going to join the server.
    pub accounts: Vec<Account>,
    pub plugins: Plugins,
    pub swarm_plugins: Plugins,
    /// The individual bot states. This must be the same length as `accounts`,
    /// since each bot gets one state.
    pub states: Vec<S>,
    pub swarm_state: SS,
    pub handle: HandleFn<Fut, S>,
    pub swarm_handle: HandleFn<Fut, S>,
}

#[derive(Error, Debug)]
pub enum SwarmStartError {
    #[error("Invalid address")]
    InvalidAddress,
    #[error(transparent)]
    ResolveAddress(#[from] ResolverError),
    #[error("Join error: {0}")]
    Join(#[from] azalea_client::JoinError),
}

/// Make a bot swarm.
pub async fn start_swarm<
    S: Send + Sync + Clone + 'static,
    SS: Send + Sync + Clone + 'static,
    A: Send + TryInto<ServerAddress>,
    Fut: Future<Output = Result<(), anyhow::Error>> + Send + 'static,
>(
    options: SwarmOptions<S, SS, A, Fut>,
) -> Result<(), SwarmStartError> {
    assert_eq!(
        options.accounts.len(),
        options.states.len(),
        "There must be exactly one state per bot."
    );

    // convert the TryInto<ServerAddress> into a ServerAddress
    let address = match options.address.try_into() {
        Ok(address) => address,
        Err(_) => return Err(SwarmStartError::InvalidAddress),
    };

    // resolve the address
    let address: ServerAddress = address.try_into().map_err(|_| JoinError::InvalidAddress)?;
    let resolved_address = resolver::resolve_address(&address).await?;

    let mut bots = try_join_all(
        options
            .accounts
            .iter()
            .map(|account| Client::join(&account, resolved_address)),
    )
    .await?;

    // extract it into two different vecs
    let (mut bots, receivers) = bots
        .into_iter()
        .unzip::<Client, UnboundedReceiver<Event>, Vec<Client>, Vec<UnboundedReceiver<Event>>>();

    for bot in &mut bots {
        // each bot has its own plugins instance, they're not shared
        let mut plugins = options.plugins.clone();
        plugins.add(bot::Plugin::default());
        bot.plugins = Arc::new(plugins);
    }

    let mut swarm = Swarm {
        bots: Arc::new(Mutex::new(bots)),
        receivers: Arc::new(Mutex::new(receivers)),
    };

    let states = options.states;
    let swarm_state = options.swarm_state;

    while let (Some(event), bot_index) = swarm.bot_recv().await {
        let bot = swarm.bots.lock()[bot_index].clone();
        let bot_state = states[bot_index].clone();
        let cloned_plugins = (*bot.plugins).clone();
        for plugin in cloned_plugins.into_iter() {
            tokio::spawn(plugin.handle(event.clone(), bot.clone()));
        }

        let bot_plugin = bot.plugins.get::<bot::Plugin>().unwrap().clone();
        tokio::spawn(bot::Plugin::handle(
            Box::new(bot_plugin),
            event.clone(),
            bot.clone(),
        ));
        tokio::spawn((options.handle)(bot, event, bot_state));
    }

    Ok(())
}

impl Swarm {
    /// Wait for any bot to get an event. We return the event and index (so we
    /// can get the state and bot from that index)
    async fn bot_recv(&mut self) -> (Option<Event>, usize) {
        let mut receivers = self.receivers.lock();
        let mut futures = Vec::with_capacity(receivers.len());
        for rx in receivers.iter_mut() {
            futures.push(rx.recv().boxed());
        }
        let (event, index, _remaining) = select_all(futures).await;
        (event, index)
    }
}