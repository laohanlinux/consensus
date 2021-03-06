use std::borrow::Cow;

use crate::{
    protocol::State,
    consensus::error::{ConsensusError, ConsensusResult},
    consensus::types::{Request, PrePrepare, Proposal},
};

use super::core::Core;
use super::preprepare::HandlePreprepare;

pub trait HandlerRequest {
    fn handle(&mut self, request: &Request<Proposal>) -> ConsensusResult;
    fn check_request_message(&self, request: &Request<Proposal>) ->ConsensusResult;
    fn accept(&mut self, request: &Request<Proposal>);
}

impl HandlerRequest for Core {
    fn handle(&mut self, request: &Request<Proposal>) -> ConsensusResult {
        self.check_request_message(request)?;
        assert_eq!(self.state, State::AcceptRequest);
        <Core as HandlerRequest>::accept(self, request);
        self.send_preprepare(request);
        Ok(())
    }

    fn check_request_message(&self, request: &Request<Proposal>) -> ConsensusResult {
        if self.current_state.height() == 0 {
            return Err(ConsensusError::WaitNewRound);
        }
        if self.current_state.height() > request.proposal.block().height() {
            return Err(ConsensusError::OldMessage);
        }
        if self.current_state.height() < request.proposal.block().height() {
            return Err(ConsensusError::FutureMessage);
        }
        Ok(())
    }

    fn accept(&mut self, request: &Request<Proposal>) {
        self.current_state.pending_request = Some(Request{proposal: request.proposal.clone()});
    }
}
