//! Publishing produced blocks to Logos from inside the node.
//!
//! The node produces blocks on its own schedule; publication follows behind.
//! A persistent cursor (`PublicationState::next_height`) records which block
//! is next, so publication resumes after a restart and never depends on the
//! node's in-memory state. Blocks are read back from the node (recent cache
//! or durable storage) and handed to an [`feeder::InscriptionSink`], which for
//! Logos is the Zone SDK sequencer ([`logos`]).

pub mod feeder;
#[cfg(feature = "logos")]
pub mod logos;
pub mod reader;
pub mod state;

pub use feeder::{BlockSource, FeederConfig, InscriptionSink, SubmitError, Submitted};
pub use state::{
    PublicationEntry, PublicationHandle, PublicationState, PublicationStatus, PublicationStore,
    SharedState,
};
