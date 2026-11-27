//! One explicitly selected root, one worker-owned memory index.
use futures_channel::oneshot;
use jwm_file_core::{Cancellation, FileIdentity, WalkOptions, WalkSummary};
use jwm_file_index::{IndexError, IndexScope, NameIndex};
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;

pub(crate) type SearchReply = Result<Vec<Vec<u8>>, IndexError>;

struct Query {
    literal: String,
    reply: oneshot::Sender<SearchReply>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Busy {
    Pending,
    Stopped,
}

pub(crate) struct IndexWorker {
    queries: SyncSender<Query>,
    cancel: Cancellation,
}

const ROOT_SLOT: i64 = 1;
const OPTIONS: WalkOptions = WalkOptions {
    entries: 25_000,
    directories: 1_024,
    depth: 24,
    show_hidden: false,
};

impl IndexWorker {
    pub(crate) fn start(
        selected_root: PathBuf,
        expected_identity: FileIdentity,
    ) -> std::io::Result<(Self, oneshot::Receiver<Result<WalkSummary, IndexError>>)> {
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let (queries, requests) = mpsc::sync_channel::<Query>(1);
        let (ready, wait) = oneshot::channel();
        thread::Builder::new()
            .name("jwm-file-index".to_owned())
            .spawn(move || {
                let result = build(&selected_root, expected_identity, &worker_cancel);
                match result {
                    Ok((index, scope, summary)) => {
                        if ready.send(Ok(summary)).is_err() {
                            return;
                        }
                        while let Ok(query) = requests.recv() {
                            if worker_cancel.check().is_err() {
                                break;
                            }
                            let result = index.search(ROOT_SLOT, scope, &query.literal);
                            if worker_cancel.check().is_err() {
                                break;
                            }
                            let _ = query.reply.send(result);
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })?;
        Ok((Self { queries, cancel }, wait))
    }

    pub(crate) fn query(&self, literal: String) -> Result<oneshot::Receiver<SearchReply>, Busy> {
        let (reply, receive) = oneshot::channel();
        self.queries
            .try_send(Query { literal, reply })
            .map_err(|error| match error {
                TrySendError::Full(_) => Busy::Pending,
                TrySendError::Disconnected(_) => Busy::Stopped,
            })?;
        Ok(receive)
    }
}

impl Drop for IndexWorker {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn build(
    root: &std::path::Path,
    identity: FileIdentity,
    cancel: &Cancellation,
) -> Result<(NameIndex, IndexScope, WalkSummary), IndexError> {
    let mut index = NameIndex::memory()?;
    let starting = index.register(ROOT_SLOT, root, identity)?;
    let (complete, summary) = index.rebuild(ROOT_SLOT, starting, OPTIONS, cancel)?;
    Ok((index, complete, summary))
}
