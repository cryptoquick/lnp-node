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

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use bitcoin::hashes::hex::{self, ToHex};
use internet2::{zmqsocket, NodeAddr, ZmqType};
use lnp::p2p::legacy::{ChannelId, TempChannelId};
use microservices::esb;
#[cfg(feature = "node")]
use microservices::node::TryService;
use strict_encoding::{strict_deserialize, strict_serialize};

use crate::i9n::ctl::CtlMsg;
use crate::i9n::rpc::{Failure, RpcMsg};
use crate::i9n::{BusMsg, ServiceBus};
use crate::{Config, Error};

#[derive(Wrapper, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, From, Default)]
#[derive(StrictEncode, StrictDecode)]
pub struct ClientName([u8; 32]);

impl Display for ClientName {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            write!(f, "{}..{}", self.0[..4].to_hex(), self.0[(self.0.len() - 4)..].to_hex())
        } else {
            f.write_str(&String::from_utf8_lossy(&self.0))
        }
    }
}

impl FromStr for ClientName {
    type Err = hex::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() > 32 {
            let mut me = Self::default();
            me.0.copy_from_slice(&s.as_bytes()[0..32]);
            Ok(me)
        } else {
            let mut me = Self::default();
            me.0[0..s.len()].copy_from_slice(s.as_bytes());
            Ok(me)
        }
    }
}

pub type ClientId = u64;

/// Identifiers of daemons participating in LNP Node
#[derive(Clone, PartialEq, Eq, Hash, Debug, Display, From, StrictEncode, StrictDecode)]
pub enum ServiceId {
    #[display("loopback")]
    Loopback,

    #[display("lnpd")]
    Lnpd,

    #[display("gossipd")]
    Gossip,

    #[display("routed")]
    Routing,

    #[display("peerd<{0}>")]
    #[from]
    Peer(NodeAddr),

    #[display("channel<{0:#x}>")]
    #[from]
    #[from(TempChannelId)]
    Channel(ChannelId),

    #[display("client<{0}>")]
    Client(ClientId),

    #[display("signer")]
    Signer,

    #[display("chain")]
    Chain,

    #[display("other<{0}>")]
    Other(ClientName),
}

impl ServiceId {
    pub fn router() -> ServiceId { ServiceId::Lnpd }

    pub fn client() -> ServiceId {
        use bitcoin::secp256k1::rand;
        ServiceId::Client(rand::random())
    }
}

impl esb::ServiceAddress for ServiceId {}

impl From<ServiceId> for Vec<u8> {
    fn from(daemon_id: ServiceId) -> Self {
        strict_serialize(&daemon_id).expect("Memory-based encoding does not fail")
    }
}

impl From<Vec<u8>> for ServiceId {
    fn from(vec: Vec<u8>) -> Self {
        strict_deserialize(&vec).unwrap_or_else(|_| {
            ServiceId::Other(
                ClientName::from_str(&String::from_utf8_lossy(&vec))
                    .expect("ClientName conversion never fails"),
            )
        })
    }
}

pub struct Service<Runtime>
where
    Runtime: esb::Handler<ServiceBus, Request = BusMsg>,
    esb::Error<ServiceId>: From<Runtime::Error>,
{
    esb: esb::Controller<ServiceBus, BusMsg, Runtime>,
    broker: bool,
}

impl<Runtime> Service<Runtime>
where
    Runtime: esb::Handler<ServiceBus, Request = BusMsg>,
    esb::Error<ServiceId>: From<Runtime::Error>,
{
    #[cfg(feature = "node")]
    pub fn run(config: Config, runtime: Runtime, broker: bool) -> Result<(), Error> {
        let service = Self::with(config, runtime, broker)?;
        service.run_loop()?;
        unreachable!()
    }

    fn with(config: Config, runtime: Runtime, broker: bool) -> Result<Self, esb::Error<ServiceId>> {
        let router = if !broker { Some(ServiceId::router()) } else { None };
        let mut services = map! {
            ServiceBus::Msg => esb::BusConfig::with_locator(
                config.msg_endpoint,
                router.clone()
            ),
            ServiceBus::Ctl => esb::BusConfig::with_locator(
                config.ctl_endpoint,
                router.clone()
            )
        };
        // Broker also listens to RPC requests from connecting clients
        if broker {
            services
                .insert(ServiceBus::Rpc, esb::BusConfig::with_locator(config.rpc_endpoint, router));
        }
        let esb = esb::Controller::with(
            services,
            runtime,
            if broker { ZmqType::RouterBind } else { ZmqType::RouterConnect },
        )?;
        Ok(Self { esb, broker })
    }

    pub fn broker(config: Config, runtime: Runtime) -> Result<Self, esb::Error<ServiceId>> {
        Self::with(config, runtime, true)
    }

    pub fn service(config: Config, runtime: Runtime) -> Result<Self, esb::Error<ServiceId>> {
        Self::with(config, runtime, false)
    }

    pub fn is_broker(&self) -> bool { self.broker }

    pub fn add_loopback(&mut self, socket: zmq::Socket) -> Result<(), esb::Error<ServiceId>> {
        self.esb.add_service_bus(ServiceBus::Bridge, esb::BusConfig {
            carrier: zmqsocket::Carrier::Socket(socket),
            router: None,
            queued: true,
        })
    }

    #[cfg(feature = "node")]
    pub fn run_loop(mut self) -> Result<(), Error> {
        if !self.is_broker() {
            std::thread::sleep(core::time::Duration::from_secs(1));
            self.esb.send_to(ServiceBus::Ctl, ServiceId::Lnpd, BusMsg::Ctl(CtlMsg::Hello))?;
            // self.esb.send_to(ServiceBus::Msg, ServiceId::Lnpd, BusMsg::Ctl(CtlMsg::Hello))?;
        }

        let identity = self.esb.handler().identity();
        info!("{} started", identity);

        self.esb.run_or_panic(&identity.to_string());

        unreachable!()
    }
}

pub type Endpoints = esb::SenderList<ServiceBus>;

pub trait TryToServiceId {
    fn try_to_service_id(&self) -> Option<ServiceId>;
}

impl TryToServiceId for ServiceId {
    fn try_to_service_id(&self) -> Option<ServiceId> { Some(self.clone()) }
}

impl TryToServiceId for &Option<ServiceId> {
    fn try_to_service_id(&self) -> Option<ServiceId> { (*self).clone() }
}

impl TryToServiceId for Option<ServiceId> {
    fn try_to_service_id(&self) -> Option<ServiceId> { self.clone() }
}

pub trait CtlServer
where
    Self: esb::Handler<ServiceBus>,
    esb::Error<ServiceId>: From<Self::Error>,
{
    /// Returns client which should receive status update reports
    #[inline]
    fn enquirer(&self) -> Option<ServiceId> { return None }

    fn report_success(
        &mut self,
        endpoints: &mut Endpoints,
        msg: Option<impl ToString>,
    ) -> Result<(), Error> {
        if let Some(ref message) = msg {
            info!("{}", message.to_string());
        }
        if let Some(dest) = self.enquirer() {
            endpoints.send_to(
                ServiceBus::Ctl,
                self.identity(),
                dest,
                BusMsg::Rpc(RpcMsg::Success(msg.map(|m| m.to_string()).into())),
            )?;
        }
        Ok(())
    }

    fn report_progress(
        &mut self,
        endpoints: &mut Endpoints,
        msg: impl ToString,
    ) -> Result<(), Error> {
        let msg = msg.to_string();
        info!("{}", msg);
        if let Some(dest) = self.enquirer() {
            endpoints.send_to(
                ServiceBus::Ctl,
                self.identity(),
                dest,
                BusMsg::Rpc(RpcMsg::Progress(msg)),
            )?;
        }
        Ok(())
    }

    fn report_failure(&mut self, endpoints: &mut Endpoints, failure: impl Into<Failure>) -> Error {
        let failure = failure.into();
        if let Some(dest) = self.enquirer() {
            // Even if we fail, we still have to terminate :)
            let _ = endpoints.send_to(
                ServiceBus::Ctl,
                self.identity(),
                dest,
                BusMsg::Rpc(RpcMsg::Failure(failure.clone())),
            );
        }
        Error::Terminate(failure.to_string())
    }

    fn send_ctl(
        &mut self,
        endpoints: &mut Endpoints,
        dest: impl TryToServiceId,
        request: CtlMsg,
    ) -> Result<(), esb::Error<ServiceId>> {
        if let Some(dest) = dest.try_to_service_id() {
            endpoints.send_to(ServiceBus::Ctl, self.identity(), dest, BusMsg::Ctl(request))?;
        }
        Ok(())
    }
}

// TODO: Move to LNP/BP Services library
use colored::Colorize;

pub trait LogStyle: ToString {
    fn promo(&self) -> colored::ColoredString { self.to_string().bold().bright_blue() }

    fn promoter(&self) -> colored::ColoredString { self.to_string().italic().bright_blue() }

    fn action(&self) -> colored::ColoredString { self.to_string().bold().yellow() }

    fn progress(&self) -> colored::ColoredString { self.to_string().bold().green() }

    fn ended(&self) -> colored::ColoredString { self.to_string().bold().bright_green() }

    fn ender(&self) -> colored::ColoredString { self.to_string().italic().bright_green() }

    fn amount(&self) -> colored::ColoredString { self.to_string().bold().bright_yellow() }

    fn addr(&self) -> colored::ColoredString { self.to_string().bold().bright_yellow() }

    fn err(&self) -> colored::ColoredString { self.to_string().bold().bright_red() }

    fn err_details(&self) -> colored::ColoredString { self.to_string().bold().red() }
}

impl<T> LogStyle for T where T: ToString {}
