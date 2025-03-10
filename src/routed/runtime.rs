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

use amplify::{Slice32, Wrapper};
use bitcoin::hashes::Hash;
use internet2::presentation::sphinx::Hop;
use lightning_invoice::Invoice;
use lnp::p2p::legacy::{Messages as LnMsg, PaymentOnion, PaymentRequest};
use lnp::router::gossip::{GossipExt, UpdateMsg};
use lnp::router::Router;
use lnp::Extension;
use lnp_rpc::{ClientId, PayInvoice, RpcMsg};
use microservices::esb;
use wallet::hlc::HashLock;

use crate::bus::{BusMsg, CtlMsg, ServiceBus};
use crate::routed::PaymentError;
use crate::rpc::ServiceId;
use crate::{Config, Endpoints, Error, Responder, Service};

pub fn run(config: Config) -> Result<(), Error> {
    let runtime =
        Runtime { identity: ServiceId::Router, router: Router::default(), enquirer: None };

    Service::run(config, runtime, false)
}

pub struct Runtime {
    identity: ServiceId,

    router: Router<GossipExt>,

    enquirer: Option<ClientId>,
}

impl Responder for Runtime {
    #[inline]
    fn enquirer(&self) -> Option<ClientId> { self.enquirer }
}

impl esb::Handler<ServiceBus> for Runtime
where
    Self: 'static,
{
    type Request = BusMsg;
    type Error = Error;

    fn identity(&self) -> ServiceId { self.identity.clone() }

    fn handle(
        &mut self,
        endpoints: &mut Endpoints,
        bus: ServiceBus,
        source: ServiceId,
        message: BusMsg,
    ) -> Result<(), Self::Error> {
        match (bus, message, source) {
            (ServiceBus::Msg, BusMsg::Ln(msg), source) => self.handle_p2p(endpoints, source, msg),
            (ServiceBus::Ctl, BusMsg::Ctl(msg), source) => self.handle_ctl(endpoints, source, msg),
            (ServiceBus::Rpc, BusMsg::Rpc(msg), ServiceId::Client(client_id)) => {
                self.handle_rpc(endpoints, client_id, msg)
            }
            (ServiceBus::Rpc, BusMsg::Rpc(_), service) => {
                unreachable!("lnpd received RPC message not from a client but from {}", service)
            }
            (bus, msg, _) => Err(Error::wrong_esb_msg(bus, &msg)),
        }
    }

    fn handle_err(
        &mut self,
        endpoints: &mut Endpoints,
        err: esb::Error<ServiceId>,
    ) -> Result<(), Self::Error> {
        let _ = self.report_failure(endpoints, &err);

        // We do nothing and do not propagate error; it's already being reported
        // with `error!` macro by the controller. If we propagate error here
        // this will make whole daemon panic
        Ok(())
    }
}

impl Runtime {
    fn handle_p2p(
        &mut self,
        _endpoints: &mut Endpoints,
        _source: ServiceId,
        message: LnMsg,
    ) -> Result<(), Error> {
        self.router.update_from_peer(&message).map_err(Error::from)
    }

    fn handle_rpc(
        &mut self,
        endpoints: &mut Endpoints,
        client_id: ClientId,
        message: RpcMsg,
    ) -> Result<(), Error> {
        match message {
            RpcMsg::PayInvoice(PayInvoice { channel_id, invoice, amount_msat }) => {
                self.enquirer = Some(client_id);
                let hash_lock =
                    HashLock::from_inner(Slice32::from_inner(invoice.payment_hash().into_inner()));
                let route = self.compute_route(endpoints, invoice, amount_msat)?;
                if route.is_empty() {
                    return Err(PaymentError::RouteNotFound.into());
                }
                let msg = CtlMsg::Payment { route, hash_lock, enquirer: client_id };
                self.send_ctl(endpoints, ServiceId::Channel(channel_id), msg)?;
            }

            wrong_msg => {
                error!("Request is not supported by the RPC interface");
                return Err(Error::wrong_esb_msg(ServiceBus::Rpc, &wrong_msg));
            }
        }

        Ok(())
    }

    fn handle_ctl(
        &mut self,
        _: &mut Endpoints,
        _: ServiceId,
        message: CtlMsg,
    ) -> Result<(), Error> {
        match message {
            CtlMsg::ChannelCreated(channel_info) => {
                debug!("Adding local channel {} to the routing table", channel_info.channel_id);
                self.router.update_from_local(&UpdateMsg::DirectChannelAdd(channel_info))?;
            }

            CtlMsg::ChannelClosed(channel_id) => {
                debug!("Removing local channel {} from the routing table", channel_id);
                self.router.update_from_local(&UpdateMsg::DirectChannelRemove(channel_id))?;
            }

            CtlMsg::ChannelBalanceUpdate { .. } => {
                // TODO: Handle balance updates
            }

            wrong_msg => {
                error!("Request {} is not supported by the CTL interface", wrong_msg);
                return Err(Error::wrong_esb_msg(ServiceBus::Ctl, &wrong_msg));
            }
        }

        Ok(())
    }

    fn compute_route(
        &mut self,
        endpoints: &mut Endpoints,
        invoice: Invoice,
        amount_msat: Option<u64>,
    ) -> Result<Vec<Hop<PaymentOnion>>, PaymentError> {
        // TODO: Add private channel information from invoice to router (use dedicated
        // PrivateRouter)

        let payment = PaymentRequest {
            amount_msat: amount_msat
                .or_else(|| invoice.amount_milli_satoshis())
                .ok_or(PaymentError::AmountUnknown)?,
            payment_hash: HashLock::from_inner(Slice32::from_inner(
                invoice.payment_hash().into_inner(),
            )),
            node_id: invoice.recover_payee_pub_key(),
            min_final_cltv_expiry: invoice.min_final_cltv_expiry() as u32,
        };
        let route = self.router.compute_route(payment);
        trace!("Computed route for the payment: {:#?}", route);
        let _ = self.report_progress(endpoints, "Route computed");

        Ok(route)
    }
}
