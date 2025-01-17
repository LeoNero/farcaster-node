// LNP Node: node running lightning network protocol and generalized lightning
// channels.
// Written in 2020 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.

//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use crate::event::{Event, StateMachine};
use crate::farcasterd::runtime::request::{
    CheckpointEntry, OfferStatusSelector, ProgressEvent, SwapProgress,
};
use crate::farcasterd::Opts;
use crate::rpc::request::{Failure, FailureCode, GetKeys, Msg, NodeInfo};
use crate::rpc::{request, Request, ServiceBus};
use crate::syncerd::{Event as SyncerEvent, SweepSuccess, TaskId};
use crate::{
    clap::Parser,
    error::SyncerError,
    rpc::request::{
        BitcoinFundingInfo, Keys, LaunchSwap, MoneroFundingInfo, OfferInfo, Outcome, Token,
    },
    service::Endpoints,
};
use crate::{Config, CtlServer, Error, LogStyle, Service, ServiceConfig, ServiceId};
use bitcoin::{hashes::hex::ToHex, secp256k1::PublicKey, secp256k1::SecretKey};
use clap::IntoApp;
use farcaster_core::{
    blockchain::{Blockchain, Network},
    swap::SwapId,
};
use farcaster_core::{role::TradeRole, swap::btcxmr::PublicOffer};
use internet2::{addr::InetSocketAddr, addr::NodeAddr, TypedEnum};
use microservices::esb::{self, Handler};
use request::List;
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::iter::FromIterator;
use std::process;
use std::time::{Duration, SystemTime};

use super::syncer_state_machine::SyncerStateMachine;
use super::trade_state_machine::TradeStateMachine;

pub fn run(
    service_config: ServiceConfig,
    config: Config,
    _opts: Opts,
    wallet_token: Token,
) -> Result<(), Error> {
    let _walletd = launch("walletd", &["--token", &wallet_token.to_string()])?;
    if config.is_grpc_enable() {
        let _grpcd = launch(
            "grpcd",
            &[
                "--grpc-port",
                &config
                    .farcasterd
                    .clone()
                    .unwrap()
                    .grpc
                    .unwrap()
                    .port
                    .to_string(),
            ],
        )?;
    }
    let empty: Vec<String> = vec![];
    let _databased = launch("databased", empty)?;

    if config.is_auto_funding_enable() {
        info!("farcasterd will attempt to fund automatically");
    }

    let runtime = Runtime {
        identity: ServiceId::Farcasterd,
        node_secret_key: None,
        node_public_key: None,
        listens: none!(),
        started: SystemTime::now(),
        spawning_services: none!(),
        registered_services: none!(),
        public_offers: none!(),
        wallet_token,
        progress: none!(),
        progress_subscriptions: none!(),
        stats: none!(),
        checkpointed_pub_offers: vec![].into(),
        config,
        syncer_task_counter: 0,
        trade_state_machines: vec![],
        syncer_state_machines: none!(),
    };

    let broker = true;
    Service::run(service_config, runtime, broker)
}

pub struct Runtime {
    identity: ServiceId,                             // Set on Runtime instantiation
    wallet_token: Token,                             // Set on Runtime instantiation
    started: SystemTime,                             // Set on Runtime instantiation
    node_secret_key: Option<SecretKey>, // Set by Keys request shortly after Hello from walletd
    node_public_key: Option<PublicKey>, // Set by Keys request shortly after Hello from walletd
    pub listens: HashSet<InetSocketAddr>, // Set by MakeOffer, contains unique socket addresses of the binding peerd listeners.
    pub spawning_services: HashSet<ServiceId>, // Services that have been launched, but have not replied with Hello yet
    pub registered_services: HashSet<ServiceId>, // Services that have announced themselves with Hello
    pub public_offers: HashSet<PublicOffer>, // The set of all known public offers. Includes open, consumed and ended offers includes open, consumed and ended offers
    progress: HashMap<ServiceId, VecDeque<Request>>, // A mapping from Swap ServiceId to its sent and received progress requests
    progress_subscriptions: HashMap<ServiceId, HashSet<ServiceId>>, // A mapping from a Client ServiceId to its subsribed swap progresses
    pub checkpointed_pub_offers: List<CheckpointEntry>, // A list of existing swap checkpoint entries that may be restored again
    pub stats: Stats,                                   // Some stats about offers and swaps
    pub config: Config, // Configuration for syncers, auto-funding, and grpc
    pub syncer_task_counter: u32, // A strictly incrementing counter of issued syncer tasks
    pub trade_state_machines: Vec<TradeStateMachine>, // New trade state machines are inserted on creation and destroyed upon state machine end transitions
    syncer_state_machines: HashMap<TaskId, SyncerStateMachine>, // New syncer state machines are inserted by their syncer task id when sending a syncer request and destroyed upon matching syncer request receival
}

impl CtlServer for Runtime {}

#[derive(Default)]
pub struct Stats {
    success: u64,
    refund: u64,
    punish: u64,
    abort: u64,
    initialized: u64,
    awaiting_funding_btc: u64,
    awaiting_funding_xmr: u64,
    funded_xmr: u64,
    funded_btc: u64,
    funding_canceled_xmr: u64,
    funding_canceled_btc: u64,
}

impl Stats {
    pub fn incr_outcome(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::Buy => self.success += 1,
            Outcome::Refund => self.refund += 1,
            Outcome::Punish => self.punish += 1,
            Outcome::Abort => self.abort += 1,
        };
    }
    pub fn incr_initiated(&mut self) {
        self.initialized += 1;
    }
    pub fn incr_awaiting_funding(&mut self, blockchain: &Blockchain) {
        match blockchain {
            Blockchain::Monero => self.awaiting_funding_xmr += 1,
            Blockchain::Bitcoin => self.awaiting_funding_btc += 1,
        }
    }
    pub fn incr_funded(&mut self, blockchain: &Blockchain) {
        match blockchain {
            Blockchain::Monero => {
                self.funded_xmr += 1;
                self.awaiting_funding_xmr -= 1;
            }
            Blockchain::Bitcoin => {
                self.funded_btc += 1;
                self.awaiting_funding_btc -= 1;
            }
        }
    }
    pub fn incr_funding_monero_canceled(&mut self) {
        self.awaiting_funding_xmr -= 1;
        self.funding_canceled_xmr += 1;
    }
    pub fn incr_funding_bitcoin_canceled(&mut self) {
        self.awaiting_funding_btc -= 1;
        self.funding_canceled_btc += 1;
    }
    pub fn success_rate(&self) -> f64 {
        let Stats {
            success,
            refund,
            punish,
            abort,
            initialized,
            awaiting_funding_btc,
            awaiting_funding_xmr,
            funded_btc,
            funded_xmr,
            funding_canceled_xmr,
            funding_canceled_btc,
        } = self;
        let total = success + refund + punish + abort;
        let rate = *success as f64 / (total as f64);
        info!(
            "Swapped({}) | Refunded({}) / Punished({}) | Aborted({}) | Initialized({}) / AwaitingFundingXMR({}) / AwaitingFundingBTC({}) / FundedXMR({}) / FundedBTC({}) / FundingCanceledXMR({}) / FundingCanceledBTC({})",
            success.bright_white_bold(),
            refund.bright_white_bold(),
            punish.bright_white_bold(),
            abort.bright_white_bold(),
            initialized,
            awaiting_funding_xmr.bright_white_bold(),
            awaiting_funding_btc.bright_white_bold(),
            funded_xmr.bright_white_bold(),
            funded_btc.bright_white_bold(),
            funding_canceled_xmr.bright_white_bold(),
            funding_canceled_btc.bright_white_bold(),
        );
        info!(
            "{} = {:>4.3}%",
            "Swap success".bright_blue_bold(),
            (rate * 100.).bright_yellow_bold(),
        );
        rate
    }
}

impl esb::Handler<ServiceBus> for Runtime {
    type Request = Request;
    type Error = Error;

    fn identity(&self) -> ServiceId {
        self.identity.clone()
    }

    fn handle(
        &mut self,
        endpoints: &mut Endpoints,
        bus: ServiceBus,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Self::Error> {
        match bus {
            ServiceBus::Msg => self.handle_rpc_msg(endpoints, source, request),
            ServiceBus::Ctl => self.handle_rpc_ctl(endpoints, source, request),
            _ => Err(Error::NotSupported(ServiceBus::Bridge, request.get_type())),
        }
    }

    fn handle_err(&mut self, _: &mut Endpoints, _: esb::Error<ServiceId>) -> Result<(), Error> {
        // We do nothing and do not propagate error; it's already being reported
        // with `error!` macro by the controller. If we propagate error here
        // this will make whole daemon panic
        Ok(())
    }
}

impl Runtime {
    fn handle_rpc_msg(
        &mut self,
        endpoints: &mut Endpoints,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        match (&request, &source) {
            (Request::Hello, _) => {
                trace!("Hello farcasterd from {}", source);
                // Ignoring; this is used to set remote identity at ZMQ level
            }
            _ => {
                self.process_request_with_state_machines(request, source, endpoints)?;
            }
        }
        Ok(())
    }

    fn handle_rpc_ctl(
        &mut self,
        endpoints: &mut Endpoints,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        let mut report_to: Vec<(Option<ServiceId>, Request)> = none!();
        match request.clone() {
            Request::Hello => {
                // Ignoring; this is used to set remote identity at ZMQ level
                info!(
                    "Service {} is now {}",
                    source.bright_white_bold(),
                    "connected".bright_green_bold()
                );

                match &source {
                    ServiceId::Farcasterd => {
                        error!(
                            "{}",
                            "Unexpected another farcasterd instance connection".err()
                        );
                    }
                    ServiceId::Database => {
                        self.registered_services.insert(source.clone());
                    }
                    ServiceId::Wallet => {
                        self.registered_services.insert(source.clone());
                        let wallet_token = GetKeys(self.wallet_token.clone());
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            self.identity(),
                            source.clone(),
                            Request::GetKeys(wallet_token),
                        )?;
                    }
                    ServiceId::Peer(connection_id) => {
                        if self.registered_services.insert(source.clone()) {
                            info!(
                                "Connection {} is registered; total {} connections are known",
                                connection_id.bright_blue_italic(),
                                self.count_connections().bright_blue_bold(),
                            );
                        } else {
                            warn!(
                                "Connection {} was already registered; the service probably was relaunched",
                                connection_id.bright_blue_italic()
                            );
                        }
                    }
                    ServiceId::Swap(_) => {
                        // nothing to do, we register swapd instances on a by-swap basis
                    }
                    ServiceId::Syncer(_, _) => {
                        if self.spawning_services.remove(&source) {
                            info!(
                                "Syncer {} is registered; total {} syncers are known",
                                source,
                                self.count_syncers().bright_blue_bold()
                            );
                            self.registered_services.insert(source.clone());
                        } else {
                            error!(
                                "Syncer {} was already registered; the service probably was relaunched\\
                                 externally, or maybe multiple syncers launched?",
                                source
                            );
                        }
                    }
                    _ => {
                        // Ignoring the rest of daemon/client types
                    }
                };

                // For the HELLO messages we have to check if any of the state machines have to be updated
                // We need to move them first in order to not retain ownership over self.
                let mut moved_trade_state_machines = self
                    .trade_state_machines
                    .drain(..)
                    .collect::<Vec<TradeStateMachine>>();
                for tsm in moved_trade_state_machines.drain(..) {
                    if let Some(new_tsm) = self.execute_trade_state_machine(
                        endpoints,
                        source.clone(),
                        request.clone(),
                        tsm,
                    )? {
                        self.trade_state_machines.push(new_tsm);
                    }
                }
                let mut moved_syncer_state_machines = self
                    .syncer_state_machines
                    .drain()
                    .collect::<Vec<(TaskId, SyncerStateMachine)>>();
                for (task_id, ssm) in moved_syncer_state_machines.drain(..) {
                    if let Some(new_ssm) = self.execute_syncer_state_machine(
                        endpoints,
                        source.clone(),
                        request.clone(),
                        ssm,
                    )? {
                        self.syncer_state_machines.insert(task_id, new_ssm);
                    }
                }
            }

            Request::Keys(Keys(sk, pk)) => {
                debug!("received peerd keys {}", sk.display_secret());
                self.node_secret_key = Some(sk);
                self.node_public_key = Some(pk);
            }

            Request::GetInfo => {
                debug!("farcasterd received GetInfo request");
                self.send_client_ctl(
                    endpoints,
                    source,
                    Request::NodeInfo(NodeInfo {
                        listens: self.listens.iter().into_iter().cloned().collect(),
                        uptime: SystemTime::now()
                            .duration_since(self.started)
                            .unwrap_or_else(|_| Duration::from_secs(0)),
                        since: self
                            .started
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_else(|_| Duration::from_secs(0))
                            .as_secs(),
                        peers: self.get_open_connections(),
                        swaps: self
                            .trade_state_machines
                            .iter()
                            .filter_map(|tsm| tsm.swap_id())
                            .collect(),
                        offers: self
                            .trade_state_machines
                            .iter()
                            .filter_map(|tsm| tsm.open_offer())
                            .collect(),
                    }),
                )?;
            }

            Request::ListPeers => {
                endpoints.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Farcasterd, // source
                    source,                // destination
                    Request::PeerList(self.get_open_connections().into()),
                )?;
            }

            Request::ListSwaps => {
                endpoints.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Farcasterd, // source
                    source,                // destination
                    Request::SwapList(
                        self.trade_state_machines
                            .iter()
                            .filter_map(|tsm| tsm.swap_id())
                            .collect(),
                    ),
                )?;
            }

            Request::ListOffers(offer_status_selector) => {
                match offer_status_selector {
                    OfferStatusSelector::Open => {
                        let open_offers = self
                            .trade_state_machines
                            .iter()
                            .filter_map(|tsm| tsm.open_offer())
                            .map(|offer| OfferInfo {
                                offer: offer.to_string(),
                                details: offer.clone(),
                            })
                            .collect();
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            ServiceId::Farcasterd, // source
                            source,                // destination
                            Request::OfferList(open_offers),
                        )?;
                    }
                    OfferStatusSelector::InProgress => {
                        let pub_offers = self
                            .public_offers
                            .iter()
                            .filter(|k| self.consumed_offers_contains(k))
                            .map(|offer| OfferInfo {
                                offer: offer.to_string(),
                                details: offer.clone(),
                            })
                            .collect();
                        endpoints.send_to(
                            ServiceBus::Ctl,
                            ServiceId::Farcasterd,
                            source,
                            Request::OfferList(pub_offers),
                        )?;
                    }
                    _ => {
                        endpoints.send_to(ServiceBus::Ctl, source, ServiceId::Database, request)?;
                    }
                };
            }

            Request::ListListens => {
                let listen_url: List<String> =
                    List::from_iter(self.listens.clone().iter().map(|listen| listen.to_string()));
                endpoints.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Farcasterd, // source
                    source,                // destination
                    Request::ListenList(listen_url),
                )?;
            }

            Request::CheckpointList(checkpointed_pub_offers) => {
                self.checkpointed_pub_offers = checkpointed_pub_offers.clone();
                endpoints.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Farcasterd,
                    source,
                    Request::CheckpointList(checkpointed_pub_offers),
                )?;
            }

            // Add progress in queues and forward to subscribed clients
            Request::Progress(..) | Request::Success(..) | Request::Failure(..) => {
                if !self.progress.contains_key(&source) {
                    self.progress.insert(source.clone(), none!());
                };
                let queue = self.progress.get_mut(&source).expect("checked/added above");
                queue.push_back(request.clone());
                // forward the request to each subscribed clients
                self.notify_subscribed_clients(endpoints, &source, &request);
            }

            // Returns a unique response that contains the complete progress queue
            Request::ReadProgress(swap_id) => {
                if let Some(queue) = self.progress.get_mut(&ServiceId::Swap(swap_id)) {
                    let mut swap_progress = SwapProgress { progress: vec![] };
                    for req in queue.iter() {
                        match req {
                            Request::Progress(request::Progress::Message(m)) => {
                                swap_progress
                                    .progress
                                    .push(ProgressEvent::Message(m.clone()));
                            }
                            Request::Progress(request::Progress::StateTransition(t)) => {
                                swap_progress
                                    .progress
                                    .push(ProgressEvent::StateTransition(t.clone()));
                            }
                            Request::Success(s) => {
                                swap_progress
                                    .progress
                                    .push(ProgressEvent::Success(s.clone()));
                            }
                            Request::Failure(f) => {
                                swap_progress
                                    .progress
                                    .push(ProgressEvent::Failure(f.clone()));
                            }
                            _ => unreachable!("not handled here"),
                        };
                    }
                    report_to.push((Some(source.clone()), Request::SwapProgress(swap_progress)));
                } else {
                    let info = if self.running_swaps_contain(&swap_id) {
                        s!("No progress made yet on this swap")
                    } else {
                        s!("Unknown swapd")
                    };
                    report_to.push((
                        Some(source.clone()),
                        Request::Failure(Failure {
                            code: FailureCode::Unknown,
                            info,
                        }),
                    ));
                }
            }

            // Add the request's source to the subscription list for later progress notifications
            // and send all notifications already in the queue
            Request::SubscribeProgress(swap_id) => {
                let service = ServiceId::Swap(swap_id);
                // if the swap is known either in the tsm's or progress, attach the client
                // otherwise terminate
                if self.running_swaps_contain(&swap_id) || self.progress.contains_key(&service) {
                    if let Some(subscribed) = self.progress_subscriptions.get_mut(&service) {
                        // ret true if not in the set, false otherwise. Double subscribe is not a
                        // problem as we manage the list in a set.
                        let _ = subscribed.insert(source.clone());
                    } else {
                        let mut subscribed = HashSet::new();
                        subscribed.insert(source.clone());
                        // None is returned, the key was not set as checked before
                        let _ = self
                            .progress_subscriptions
                            .insert(service.clone(), subscribed);
                    }
                    trace!(
                        "{} has been added to {} progress subscription",
                        source.clone(),
                        swap_id
                    );
                    // send all queued notification to the source to catch up
                    if let Some(queue) = self.progress.get_mut(&service) {
                        for req in queue.iter() {
                            report_to.push((Some(source.clone()), req.clone()));
                        }
                    }
                } else {
                    // no swap service exists, terminate
                    report_to.push((
                        Some(source.clone()),
                        Request::Failure(Failure {
                            code: FailureCode::Unknown,
                            info: "Unknown swapd".to_string(),
                        }),
                    ));
                }
            }

            // Remove the request's source from the subscription list of notifications
            Request::UnsubscribeProgress(swap_id) => {
                let service = ServiceId::Swap(swap_id);
                if let Some(subscribed) = self.progress_subscriptions.get_mut(&service) {
                    // we don't care if the source was not in the set
                    let _ = subscribed.remove(&source);
                    trace!(
                        "{} has been removed from {} progress subscription",
                        source.clone(),
                        swap_id
                    );
                    if subscribed.is_empty() {
                        // we drop the empty set located at the swap index
                        let _ = self.progress_subscriptions.remove(&service);
                    }
                }
                // if no swap service exists no subscription need to be removed
            }

            Request::NeedsFunding(Blockchain::Monero) => {
                let funding_infos: Vec<MoneroFundingInfo> = self
                    .trade_state_machines
                    .iter()
                    .filter_map(|tsm| tsm.needs_funding_monero())
                    .collect();
                let len = funding_infos.len();
                let res = funding_infos
                    .iter()
                    .enumerate()
                    .map(|(i, funding_info)| {
                        let mut res = format!("{}", funding_info);
                        if i < len - 1 {
                            res.push('\n');
                        }
                        res
                    })
                    .collect();
                endpoints.send_to(
                    ServiceBus::Ctl,
                    self.identity(),
                    source,
                    Request::String(res),
                )?;
            }
            Request::NeedsFunding(Blockchain::Bitcoin) => {
                let funding_infos: Vec<BitcoinFundingInfo> = self
                    .trade_state_machines
                    .iter()
                    .filter_map(|tsm| tsm.needs_funding_bitcoin())
                    .collect();
                let len = funding_infos.len();
                let res = funding_infos
                    .iter()
                    .enumerate()
                    .map(|(i, funding_info)| {
                        let mut res = format!("{}", funding_info);
                        if i < len - 1 {
                            res.push('\n');
                        }
                        res
                    })
                    .collect();
                endpoints.send_to(
                    ServiceBus::Ctl,
                    self.identity(),
                    source,
                    Request::String(res),
                )?;
            }

            Request::PeerdTerminated => {
                if let ServiceId::Peer(addr) = source {
                    if self.registered_services.remove(&source) {
                        debug!(
                            "removed connection {} from farcasterd registered connections",
                            addr
                        );

                        // log a message if a swap running over this connection
                        // is not completed, and thus present in consumed_offers
                        let peerd_id = ServiceId::Peer(addr);
                        if self.connection_has_swap_client(&peerd_id) {
                            info!("a swap is still running over the terminated peer {}, the counterparty will attempt to reconnect.", addr);
                        }
                    }
                }
            }

            _ => {
                self.process_request_with_state_machines(request, source, endpoints)?;
            }
        }

        for (i, (respond_to, resp)) in report_to.clone().into_iter().enumerate() {
            if let Some(respond_to) = respond_to {
                // do not respond to self
                if respond_to == self.identity() {
                    continue;
                }
                trace!(
                    "(#{}) Respond to {}: {}",
                    i,
                    respond_to.bright_yellow_bold(),
                    resp.bright_blue_bold(),
                );
                endpoints.send_to(ServiceBus::Ctl, self.identity(), respond_to, resp)?;
            }
        }
        trace!("Processed all cli notifications");

        Ok(())
    }

    pub fn services_ready(&self) -> Result<(), Error> {
        if !self.registered_services.contains(&ServiceId::Wallet) {
            Err(Error::Farcaster(
                "Farcaster not ready yet, walletd still starting".to_string(),
            ))
        } else if !self.registered_services.contains(&ServiceId::Database) {
            Err(Error::Farcaster(
                "Farcaster not ready yet, databased still starting".to_string(),
            ))
        } else {
            Ok(())
        }
    }
    pub fn peer_keys_ready(&self) -> Result<(SecretKey, PublicKey), Error> {
        if let (Some(sk), Some(pk)) = (self.node_secret_key, self.node_public_key) {
            Ok((sk, pk))
        } else {
            Err(Error::Farcaster("Peer keys not ready yet".to_string()))
        }
    }
    pub fn clean_up_after_swap(
        &mut self,
        swap_id: &SwapId,
        endpoints: &mut Endpoints,
    ) -> Result<(), Error> {
        endpoints.send_to(
            ServiceBus::Ctl,
            self.identity(),
            ServiceId::Swap(*swap_id),
            Request::Terminate,
        )?;
        endpoints.send_to(
            ServiceBus::Ctl,
            self.identity(),
            ServiceId::Database,
            Request::RemoveCheckpoint(*swap_id),
        )?;

        self.registered_services = self
            .registered_services
            .clone()
            .drain()
            .filter(|service| {
                if let ServiceId::Peer(..) = service {
                    if !self.connection_has_swap_client(service) {
                        endpoints
                            .send_to(
                                ServiceBus::Ctl,
                                self.identity(),
                                service.clone(),
                                Request::Terminate,
                            )
                            .is_err()
                    } else {
                        true
                    }
                } else if let ServiceId::Syncer(..) = service {
                    if !self.syncer_has_client(service) {
                        info!("Terminating {}", service);
                        endpoints
                            .send_to(
                                ServiceBus::Ctl,
                                self.identity(),
                                service.clone(),
                                Request::Terminate,
                            )
                            .is_err()
                    } else {
                        true
                    }
                } else {
                    true
                }
            })
            .collect();
        Ok(())
    }

    fn consumed_offers_contains(&self, offer: &PublicOffer) -> bool {
        self.trade_state_machines
            .iter()
            .filter_map(|tsm| tsm.consumed_offer())
            .any(|tsm_offer| tsm_offer.offer.id() == offer.offer.id())
    }

    fn running_swaps_contain(&self, swap_id: &SwapId) -> bool {
        self.trade_state_machines
            .iter()
            .filter_map(|tsm| tsm.swap_id())
            .any(|tsm_swap_id| tsm_swap_id == *swap_id)
    }

    pub fn syncer_has_client(&self, syncerd: &ServiceId) -> bool {
        self.trade_state_machines.iter().any(|tsm| {
            tsm.syncers()
                .iter()
                .any(|client_syncer| client_syncer == syncerd)
        }) || self
            .syncer_state_machines
            .values()
            .filter_map(|ssm| ssm.syncer())
            .any(|client_syncer| client_syncer == *syncerd)
    }

    fn count_syncers(&self) -> usize {
        self.registered_services
            .iter()
            .filter(|s| matches!(s, ServiceId::Syncer(..)))
            .count()
    }

    fn connection_has_swap_client(&self, peerd: &ServiceId) -> bool {
        self.trade_state_machines
            .iter()
            .filter_map(|tsm| tsm.get_connection())
            .any(|client_connection| client_connection == *peerd)
    }

    fn count_connections(&self) -> usize {
        self.registered_services
            .iter()
            .filter(|s| matches!(s, ServiceId::Peer(..)))
            .count()
    }

    fn get_open_connections(&self) -> Vec<NodeAddr> {
        self.registered_services
            .iter()
            .filter_map(|s| {
                if let ServiceId::Peer(n) = s {
                    Some(*n)
                } else {
                    None
                }
            })
            .collect()
    }

    fn match_request_to_syncer_state_machine(
        &mut self,
        req: Request,
        source: ServiceId,
    ) -> Result<Option<SyncerStateMachine>, Error> {
        match (req, source) {
            (Request::SweepAddress(..), _) => Ok(Some(SyncerStateMachine::Start)),
            (Request::SyncerEvent(SyncerEvent::SweepSuccess(SweepSuccess { id, .. })), _) => {
                Ok(self.syncer_state_machines.remove(&id))
            }
            _ => Ok(None),
        }
    }

    fn match_request_to_trade_state_machine(
        &mut self,
        req: Request,
        source: ServiceId,
    ) -> Result<Option<TradeStateMachine>, Error> {
        match (req, source) {
            (Request::RestoreCheckpoint(..), _) => Ok(Some(TradeStateMachine::StartRestore)),
            (Request::MakeOffer(..), _) => Ok(Some(TradeStateMachine::StartMaker)),
            (Request::TakeOffer(..), _) => Ok(Some(TradeStateMachine::StartTaker)),
            (Request::Protocol(Msg::TakerCommit(request::TakeCommit { public_offer, .. })), _)
            | (Request::RevokeOffer(public_offer), _) => Ok(self
                .trade_state_machines
                .iter()
                .position(|tsm| {
                    if let Some(tsm_public_offer) = tsm.open_offer() {
                        tsm_public_offer == public_offer
                    } else {
                        false
                    }
                })
                .map(|pos| self.trade_state_machines.remove(pos))),
            (Request::LaunchSwap(LaunchSwap { public_offer, .. }), _) => Ok(self
                .trade_state_machines
                .iter()
                .position(|tsm| {
                    if let Some(tsm_public_offer) = tsm.consumed_offer() {
                        tsm_public_offer == public_offer
                    } else {
                        false
                    }
                })
                .map(|pos| self.trade_state_machines.remove(pos))),
            (Request::PeerdUnreachable(..), ServiceId::Swap(swap_id))
            | (Request::FundingInfo(..), ServiceId::Swap(swap_id))
            | (Request::FundingCanceled(..), ServiceId::Swap(swap_id))
            | (Request::FundingCompleted(..), ServiceId::Swap(swap_id))
            | (Request::SwapOutcome(..), ServiceId::Swap(swap_id)) => Ok(self
                .trade_state_machines
                .iter()
                .position(|tsm| {
                    if let Some(tsm_swap_id) = tsm.swap_id() {
                        tsm_swap_id == swap_id
                    } else {
                        false
                    }
                })
                .map(|pos| self.trade_state_machines.remove(pos))),
            _ => Ok(None),
        }
    }

    fn process_request_with_state_machines(
        &mut self,
        request: Request,
        source: ServiceId,
        endpoints: &mut Endpoints,
    ) -> Result<(), Error> {
        if let Some(tsm) =
            self.match_request_to_trade_state_machine(request.clone(), source.clone())?
        {
            if let Some(new_tsm) =
                self.execute_trade_state_machine(endpoints, source, request, tsm)?
            {
                self.trade_state_machines.push(new_tsm);
            }
            Ok(())
        } else if let Some(ssm) =
            self.match_request_to_syncer_state_machine(request.clone(), source.clone())?
        {
            if let Some(new_ssm) =
                self.execute_syncer_state_machine(endpoints, source, request, ssm)?
            {
                if let Some(task_id) = new_ssm.task_id() {
                    self.syncer_state_machines.insert(task_id, new_ssm);
                } else {
                    error!("Cannot process new syncer state machine without a task id");
                }
            }
            Ok(())
        } else {
            warn!("Received request {}, but did not process it", request);
            Ok(())
        }
    }

    fn execute_syncer_state_machine(
        &mut self,
        endpoints: &mut Endpoints,
        source: ServiceId,
        request: Request,
        ssm: SyncerStateMachine,
    ) -> Result<Option<SyncerStateMachine>, Error> {
        let event = Event::with(endpoints, self.identity(), source, request);
        let ssm_display = ssm.to_string();
        if let Some(new_ssm) = ssm.next(event, self)? {
            let new_ssm_display = new_ssm.to_string();
            // relegate state transitions staying the same to debug
            if new_ssm_display == ssm_display {
                debug!(
                    "Syncer state self transition {}",
                    new_ssm.bright_green_bold()
                );
            } else {
                info!(
                    "Syncer state transition {} -> {}",
                    ssm_display.red_bold(),
                    new_ssm.bright_green_bold()
                );
            }
            Ok(Some(new_ssm))
        } else {
            info!(
                "Syncer state machine ended {} -> {}",
                ssm_display.red_bold(),
                "End".to_string().bright_green_bold()
            );
            Ok(None)
        }
    }

    fn execute_trade_state_machine(
        &mut self,
        endpoints: &mut Endpoints,
        source: ServiceId,
        request: Request,
        tsm: TradeStateMachine,
    ) -> Result<Option<TradeStateMachine>, Error> {
        let event = Event::with(endpoints, self.identity(), source, request);
        let tsm_display = tsm.to_string();
        if let Some(new_tsm) = tsm.next(event, self)? {
            let new_tsm_display = new_tsm.to_string();
            // relegate state transitions staying the same to debug
            if new_tsm_display == tsm_display {
                debug!(
                    "Trade state self transition {}",
                    new_tsm.bright_green_bold()
                );
            } else {
                info!(
                    "Trade state transition {} -> {}",
                    tsm_display.red_bold(),
                    new_tsm.bright_green_bold()
                );
            }
            Ok(Some(new_tsm))
        } else {
            info!(
                "Trade state machine ended {} -> {}",
                tsm_display.red_bold(),
                "End".to_string().bright_green_bold()
            );
            Ok(None)
        }
    }

    pub fn listen(&mut self, addr: NodeAddr, sk: SecretKey) -> Result<(), Error> {
        let address = addr.addr.address();
        let port = addr.addr.port().ok_or(Error::Farcaster(
            "listen requires the port to listen on".to_string(),
        ))?;

        debug!("Instantiating peerd...");
        let child = launch(
            "peerd",
            &[
                "--listen",
                &format!("{}", address),
                "--port",
                &port.to_string(),
                "--peer-secret-key",
                &format!("{}", sk.display_secret()),
                "--token",
                &self.wallet_token.clone().to_string(),
            ],
        );

        // in case it can't connect wait for it to crash
        std::thread::sleep(Duration::from_secs_f32(0.5));

        // status is Some if peerd returns because it crashed
        let (child, status) = child.and_then(|mut c| c.try_wait().map(|s| (c, s)))?;

        if status.is_some() {
            return Err(Error::Peer(internet2::presentation::Error::InvalidEndpoint));
        }

        debug!("New instance of peerd launched with PID {}", child.id());
        Ok(())
    }

    pub fn connect_peer(&mut self, node_addr: &NodeAddr, sk: SecretKey) -> Result<(), Error> {
        debug!("Instantiating peerd...");
        if self
            .registered_services
            .contains(&ServiceId::Peer(*node_addr))
        {
            return Err(Error::Other(format!(
                "Already connected to peer {}",
                node_addr
            )));
        }

        // Start peerd
        let child = launch(
            "peerd",
            &[
                "--connect",
                &node_addr.to_string(),
                "--peer-secret-key",
                &format!("{}", sk.display_secret()),
                "--token",
                &self.wallet_token.clone().to_string(),
            ],
        );

        // in case it can't connect wait for it to crash
        std::thread::sleep(Duration::from_secs_f32(0.5));

        // status is Some if peerd returns because it crashed
        let (child, status) = child.and_then(|mut c| c.try_wait().map(|s| (c, s)))?;

        if status.is_some() {
            return Err(Error::Peer(internet2::presentation::Error::InvalidEndpoint));
        }

        debug!("New instance of peerd launched with PID {}", child.id());

        self.spawning_services.insert(ServiceId::Peer(*node_addr));
        debug!("Awaiting for peerd to connect...");

        Ok(())
    }

    /// Notify(forward to) the subscribed clients still online with the given request
    fn notify_subscribed_clients(
        &mut self,
        endpoints: &mut Endpoints,
        source: &ServiceId,
        request: &Request,
    ) {
        // if subs exists for the source (swap_id), forward the request to every subs
        if let Some(subs) = self.progress_subscriptions.get_mut(source) {
            // if the sub is no longer reachable, i.e. the process terminated without calling
            // unsub, remove it from sub list
            subs.retain(|sub| {
                endpoints
                    .send_to(
                        ServiceBus::Ctl,
                        ServiceId::Farcasterd,
                        sub.clone(),
                        request.clone(),
                    )
                    .is_ok()
            });
        }
    }
}

pub fn syncer_up(
    spawning_services: &mut HashSet<ServiceId>,
    registered_services: &mut HashSet<ServiceId>,
    blockchain: Blockchain,
    network: Network,
    config: &Config,
) -> Result<Option<ServiceId>, Error> {
    let syncer_service = ServiceId::Syncer(blockchain, network);
    if !registered_services.contains(&syncer_service)
        && !spawning_services.contains(&syncer_service)
    {
        let mut args = vec![
            "--blockchain".to_string(),
            blockchain.to_string(),
            "--network".to_string(),
            network.to_string(),
        ];
        args.append(&mut syncer_servers_args(config, blockchain, network)?);
        info!("launching syncer with: {:?}", args);
        launch("syncerd", args)?;
        spawning_services.insert(syncer_service.clone());
    }
    if registered_services.contains(&syncer_service) {
        Ok(Some(syncer_service))
    } else {
        Ok(None)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn launch_swapd(
    local_trade_role: TradeRole,
    public_offer: PublicOffer,
    swap_id: SwapId,
) -> Result<String, Error> {
    debug!("Instantiating swapd...");
    let child = launch(
        "swapd",
        &[
            swap_id.to_hex(),
            public_offer.to_string(),
            local_trade_role.to_string(),
        ],
    )?;
    let msg = format!("New instance of swapd launched with PID {}", child.id());
    debug!("{}", msg);
    debug!("Awaiting for swapd to connect...");
    Ok(msg)
}

/// Return the list of needed arguments for a syncer given a config and a network.
/// This function only register the minimal set of URLs needed for the blockchain to work.
fn syncer_servers_args(
    config: &Config,
    blockchain: Blockchain,
    net: Network,
) -> Result<Vec<String>, Error> {
    match config.get_syncer_servers(net) {
        Some(servers) => match blockchain {
            Blockchain::Bitcoin => Ok(vec![
                "--electrum-server".to_string(),
                servers.electrum_server,
            ]),
            Blockchain::Monero => {
                let mut args: Vec<String> = vec![
                    "--monero-daemon".to_string(),
                    servers.monero_daemon,
                    "--monero-rpc-wallet".to_string(),
                    servers.monero_rpc_wallet,
                ];
                args.extend(
                    servers
                        .monero_lws
                        .map_or(vec![], |v| vec!["--monero-lws".to_string(), v]),
                );
                args.extend(
                    servers
                        .monero_wallet_dir
                        .map_or(vec![], |v| vec!["--monero-wallet-dir-path".to_string(), v]),
                );
                Ok(args)
            }
        },
        None => Err(SyncerError::InvalidConfig.into()),
    }
}

pub fn launch(
    name: &str,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> io::Result<process::Child> {
    let app = Opts::command();
    let mut bin_path = std::env::current_exe().map_err(|err| {
        error!("Unable to detect binary directory: {}", err);
        err
    })?;
    bin_path.pop();

    bin_path.push(name);
    #[cfg(target_os = "windows")]
    bin_path.set_extension("exe");

    debug!(
        "Launching {} as a separate process using `{}` as binary",
        name,
        bin_path.to_string_lossy()
    );

    let mut cmd = process::Command::new(bin_path);

    // Forwarded shared options from farcasterd to launched microservices
    // Cannot use value_of directly because of default values
    let matches = app.get_matches();

    if let Some(d) = &matches.value_of("data-dir") {
        cmd.args(&["-d", d]);
    }

    if let Some(m) = &matches.value_of("msg-socket") {
        cmd.args(&["-m", m]);
    }

    if let Some(x) = &matches.value_of("ctl-socket") {
        cmd.args(&["-x", x]);
    }

    // Forward tor proxy argument
    let parsed = Opts::parse();
    info!("tor opts: {:?}", parsed.shared.tor_proxy);
    if let Some(t) = &matches.value_of("tor-proxy") {
        cmd.args(&["-T", *t]);
    }

    // Given specialized args in launch
    cmd.args(args);

    debug!("Executing `{:?}`", cmd);
    cmd.spawn().map_err(|err| {
        error!("Error launching {}: {}", name, err);
        err
    })
}
