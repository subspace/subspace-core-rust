use crate::{crypto, Piece, Tag, PIECE_SIZE};
use async_std::fs::OpenOptions;
use async_std::path::PathBuf;
use async_std::task;
use futures::channel::mpsc;
use futures::channel::mpsc::Sender;
use futures::channel::mpsc::UnboundedSender;
use futures::channel::oneshot;
use futures::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SinkExt, StreamExt};
use log::*;
use rocksdb::IteratorMode;
use rocksdb::DB;
use std::convert::TryInto;
use std::io;
use std::io::SeekFrom;
use std::ops::Deref;
use std::sync::Arc;

#[derive(Debug)]
pub enum PlotCreationError {
    PlotOpen(io::Error),
    PlotMapOpen(rocksdb::Error),
    PlotTagsOpen(rocksdb::Error),
    MapRead(io::Error),
}

#[derive(Debug)]
enum ReadRequests {
    IsEmpty {
        result_sender: oneshot::Sender<bool>,
    },
    ReadEncoding {
        index: usize,
        result_sender: oneshot::Sender<io::Result<Piece>>,
    },
    FindByTag {
        tag: u64,
        result_sender: oneshot::Sender<io::Result<(u64, usize)>>,
    },
    FindByRange {
        target: Tag,
        range: u64,
        result_sender: oneshot::Sender<io::Result<Vec<(Tag, usize)>>>,
    },
    GetKeys {
        result_sender: oneshot::Sender<io::Result<Vec<u64>>>,
    },
}

#[derive(Debug)]
enum WriteRequests {
    WriteEncoding {
        encoding: Piece,
        nonce: u64,
        index: usize,
        result_sender: oneshot::Sender<io::Result<()>>,
    },
    RemoveEncoding {
        index: usize,
        result_sender: oneshot::Sender<io::Result<()>>,
    },
}

pub struct Inner {
    any_requests_sender: Sender<()>,
    read_requests_sender: UnboundedSender<ReadRequests>,
    write_requests_sender: UnboundedSender<WriteRequests>,
}

/* ToDo
 *
 * Return result for solve()
 * Delete entire plot (perhaps with script) for testing
 * Extend tests
 * Resize plot by removing the last x indices and adjusting struct params
*/

#[derive(Clone)]
pub struct Plot {
    inner: Arc<Inner>,
}

impl Plot {
    /// Creates a new plot for persisting encoded pieces to disk
    pub async fn open_or_create(path: &PathBuf) -> Result<Plot, PlotCreationError> {
        let mut plot_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path.join("plot.bin"))
            .await
            .map_err(PlotCreationError::PlotOpen)?;

        let map_db = Arc::new(
            // DB::open_default(path.join("plot-map.rocksdb").to_str().unwrap())
            DB::open_default(path.join("plot-map")).map_err(PlotCreationError::PlotMapOpen)?,
        );

        let tags_db = Arc::new(
            // DB::open_default(path.join("plot-map.rocksdb").to_str().unwrap())
            DB::open_default(path.join("plot-tags")).map_err(PlotCreationError::PlotTagsOpen)?,
        );

        // Channel with at most single element to throttle loop below if there are no updates
        let (any_requests_sender, mut any_requests_receiver) = mpsc::channel::<()>(1);
        let (read_requests_sender, mut read_requests_receiver) = mpsc::unbounded::<ReadRequests>();
        let (write_requests_sender, mut write_requests_receiver) =
            mpsc::unbounded::<WriteRequests>();

        // TODO: Handle drop nicer: when read is dropped, make sure writes still all finish
        task::spawn(async move {
            let mut did_nothing = true;
            loop {
                if did_nothing {
                    // Wait for stuff to come in
                    if any_requests_receiver.next().await.is_none() {
                        return;
                    }
                }

                did_nothing = true;

                // Process as many read requests as there is
                while let Ok(read_request) = read_requests_receiver.try_next() {
                    did_nothing = false;

                    match read_request {
                        Some(ReadRequests::IsEmpty { result_sender }) => {
                            let _ = result_sender.send(
                                task::spawn_blocking({
                                    let map_db = Arc::clone(&map_db);
                                    move || map_db.iterator(IteratorMode::Start).next().is_none()
                                })
                                .await,
                            );
                        }
                        Some(ReadRequests::ReadEncoding {
                            index,
                            result_sender,
                        }) => {
                            // TODO: Remove unwrap
                            let position = task::spawn_blocking({
                                let map_db = Arc::clone(&map_db);
                                move || map_db.get(index.to_le_bytes())
                            })
                            .await
                            .unwrap()
                            .map(|position| {
                                u64::from_le_bytes(position.as_slice().try_into().unwrap())
                            });
                            let _ = result_sender.send(match position {
                                Some(position) => {
                                    try {
                                        plot_file.seek(SeekFrom::Start(position)).await?;
                                        let mut buffer = [0u8; PIECE_SIZE];
                                        async_std::io::ReadExt::read_exact(
                                            &mut plot_file,
                                            &mut buffer,
                                        )
                                        .await?;
                                        buffer
                                    }
                                }
                                None => Err(io::Error::from(io::ErrorKind::NotFound)),
                            });
                        }
                        None => {
                            return;
                        }
                        Some(ReadRequests::FindByTag { tag, result_sender }) => {
                            // TODO: Remove unwrap
                            let (best_tag, index) = task::spawn_blocking({
                                let tags_db = Arc::clone(&tags_db);
                                move || {
                                    let mut iter = tags_db.raw_iterator();

                                    iter.seek(tag.to_le_bytes());
                                    // TODO: Remove unwrap
                                    let best_tag = iter.key().unwrap();
                                    let index = iter.value().unwrap();

                                    (
                                        u64::from_le_bytes(best_tag.try_into().unwrap()),
                                        usize::from_le_bytes(index.try_into().unwrap()),
                                    )
                                }
                            })
                            .await;

                            let _ = result_sender.send(Ok((best_tag, index)));
                        }
                        Some(ReadRequests::FindByRange {
                            target,
                            range,
                            result_sender,
                        }) => {
                            // TODO: Remove unwrap
                            let solutions = task::spawn_blocking({
                                let tags_db = Arc::clone(&tags_db);
                                move || {
                                    let mut iter = tags_db.raw_iterator();

                                    let mut solutions: Vec<(Tag, usize)> = Vec::new();

                                    let (lower, is_lower_overflowed) =
                                        u64::from_be_bytes(target).overflowing_sub(range / 2);
                                    let (upper, is_upper_overflowed) =
                                        u64::from_be_bytes(target).overflowing_add(range / 2);

                                    trace!(
                                        "{} Lower overflow: {} -- Upper overflow: {}",
                                        hex::encode(&target),
                                        is_lower_overflowed,
                                        is_upper_overflowed
                                    );

                                    if is_lower_overflowed || is_upper_overflowed {
                                        iter.seek_to_first();
                                        while let Some(tag) = iter.key() {
                                            let tag = tag.try_into().unwrap();
                                            let index = iter.value().unwrap();
                                            if u64::from_be_bytes(tag) <= upper {
                                                solutions.push((
                                                    tag,
                                                    usize::from_le_bytes(index.try_into().unwrap()),
                                                ));
                                                iter.next();
                                            } else {
                                                break;
                                            }
                                        }
                                        iter.seek(lower.to_be_bytes());
                                        while let Some(tag) = iter.key() {
                                            let tag = tag.try_into().unwrap();
                                            let index = iter.value().unwrap();

                                            solutions.push((
                                                tag,
                                                usize::from_le_bytes(index.try_into().unwrap()),
                                            ));
                                            iter.next();
                                        }
                                    } else {
                                        iter.seek(lower.to_be_bytes());
                                        while let Some(tag) = iter.key() {
                                            let tag = tag.try_into().unwrap();
                                            let index = iter.value().unwrap();
                                            if u64::from_be_bytes(tag) <= upper {
                                                solutions.push((
                                                    tag,
                                                    usize::from_le_bytes(index.try_into().unwrap()),
                                                ));
                                                iter.next();
                                            } else {
                                                break;
                                            }
                                        }
                                    }

                                    solutions
                                }
                            })
                            .await;

                            let _ = result_sender.send(Ok(solutions));
                        }
                        Some(ReadRequests::GetKeys { result_sender }) => {
                            // TODO: Remove unwrap
                            let keys = task::spawn_blocking({
                                let tags_db = Arc::clone(&tags_db);
                                move || {
                                    let mut iter = tags_db.raw_iterator();
                                    let mut keys: Vec<u64> = Vec::new();

                                    iter.seek_to_first();
                                    while iter.key().is_some() {
                                        keys.push(u64::from_be_bytes(
                                            iter.key().unwrap().try_into().unwrap(),
                                        ));
                                        iter.next();
                                    }

                                    keys
                                }
                            })
                            .await;

                            let _ = result_sender.send(Ok(keys));
                        }
                    }
                }

                let write_request = write_requests_receiver.try_next();
                if write_request.is_ok() {
                    did_nothing = false;
                }
                // Process at most write request since reading is higher priority
                match write_request {
                    Ok(Some(WriteRequests::WriteEncoding {
                        index,
                        nonce,
                        encoding,
                        result_sender,
                    })) => {
                        // TODO: remove unwrap
                        task::spawn_blocking({
                            let map_db = Arc::clone(&map_db);
                            move || map_db.delete(index.to_le_bytes())
                        })
                        .await
                        .unwrap();

                        let _ = result_sender.send(
                            try {
                                let position = plot_file.seek(SeekFrom::Current(0)).await?;
                                AsyncWriteExt::write_all(&mut plot_file, &encoding).await?;

                                // TODO: remove unwrap
                                task::spawn_blocking({
                                    let map_db = Arc::clone(&map_db);
                                    let tags_db = Arc::clone(&tags_db);
                                    let tag = crypto::create_hmac(&encoding, &nonce.to_le_bytes());
                                    move || {
                                        tags_db.put(&tag[0..8], index.to_le_bytes()).and_then(
                                            |_| {
                                                map_db.put(
                                                    index.to_le_bytes(),
                                                    position.to_le_bytes(),
                                                )
                                            },
                                        )
                                    }
                                })
                                .await
                                .unwrap();
                            },
                        );
                    }
                    Ok(Some(WriteRequests::RemoveEncoding {
                        index,
                        result_sender,
                    })) => {
                        // TODO: remove unwrap
                        task::spawn_blocking({
                            let map_db = Arc::clone(&map_db);
                            move || map_db.delete(index.to_le_bytes())
                        })
                        .await
                        .unwrap();

                        let _ = result_sender.send(Ok(()));
                    }
                    Ok(None) => {
                        return;
                    }
                    Err(_) => {
                        // Ignore
                    }
                }
            }
        });

        let inner = Inner {
            any_requests_sender,
            read_requests_sender,
            write_requests_sender,
        };

        Ok(Plot {
            inner: Arc::new(inner),
        })
    }

    pub async fn is_empty(&self) -> bool {
        let (result_sender, result_receiver) = oneshot::channel();

        self.read_requests_sender
            .clone()
            .send(ReadRequests::IsEmpty { result_sender })
            .await
            .expect("Failed sending read request");

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.any_requests_sender.clone().try_send(());

        result_receiver
            .await
            .expect("Read result sender was dropped")
    }

    /// Reads a piece from plot by index
    pub async fn read(&self, index: usize) -> io::Result<Piece> {
        let (result_sender, result_receiver) = oneshot::channel();

        self.read_requests_sender
            .clone()
            .send(ReadRequests::ReadEncoding {
                index,
                result_sender,
            })
            .await
            .expect("Failed sending read encoding request");

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.any_requests_sender.clone().try_send(());

        result_receiver
            .await
            .expect("Read encoding result sender was dropped")
    }

    pub async fn find_by_tag(&self, tag: u64) -> io::Result<(u64, usize)> {
        let (result_sender, result_receiver) = oneshot::channel();

        self.read_requests_sender
            .clone()
            .send(ReadRequests::FindByTag { tag, result_sender })
            .await
            .expect("Failed sending get by tag request");

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.any_requests_sender.clone().try_send(());

        result_receiver
            .await
            .expect("Get by tag result sender was dropped")
    }

    pub async fn find_by_range(
        &self,
        target: [u8; 8],
        range: u64,
    ) -> io::Result<Vec<(Tag, usize)>> {
        let (result_sender, result_receiver) = oneshot::channel();

        self.read_requests_sender
            .clone()
            .send(ReadRequests::FindByRange {
                target,
                range,
                result_sender,
            })
            .await
            .expect("Failed sending get by range request");

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.any_requests_sender.clone().try_send(());

        result_receiver
            .await
            .expect("Get by range result sender was dropped")
    }

    pub async fn get_keys(&self) -> io::Result<Vec<u64>> {
        let (result_sender, result_receiver) = oneshot::channel();

        self.read_requests_sender
            .clone()
            .send(ReadRequests::GetKeys { result_sender })
            .await
            .expect("Failed sending get keys request");

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.any_requests_sender.clone().try_send(());

        result_receiver
            .await
            .expect("Get keys result sender was dropped")
    }

    /// Writes a piece to the plot by index, will overwrite if piece exists (updates)
    pub async fn write(&self, encoding: Piece, nonce: u64, index: usize) -> io::Result<()> {
        let (result_sender, result_receiver) = oneshot::channel();

        self.write_requests_sender
            .clone()
            .send(WriteRequests::WriteEncoding {
                encoding,
                nonce,
                index,
                result_sender,
            })
            .await
            .expect("Failed sending write encoding request");

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.any_requests_sender.clone().try_send(());

        result_receiver
            .await
            .expect("Write encoding result sender was dropped")
    }

    /// Removes a piece from the plot by index, by deleting its index from the map
    pub async fn remove(&self, index: usize) -> io::Result<()> {
        let (result_sender, result_receiver) = oneshot::channel();

        self.write_requests_sender
            .clone()
            .send(WriteRequests::RemoveEncoding {
                index,
                result_sender,
            })
            .await
            .expect("Failed sending remove encoding request");

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.any_requests_sender.clone().try_send(());

        result_receiver
            .await
            .expect("Remove encoding result sender was dropped")
    }
}

impl Deref for Plot {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;
    use async_std::path::PathBuf;
    use rand::prelude::*;

    #[async_std::test]
    async fn test_basic() {
        let path = PathBuf::from("target").join("test");

        let piece = crypto::generate_random_piece();
        let tag = rand::thread_rng().gen::<u64>();
        let index = 0;

        let plot = Plot::open_or_create(&path).await.unwrap();
        plot.write(piece, tag, index).await.unwrap();
        let extracted_piece = plot.read(index).await.unwrap();

        assert_eq!(extracted_piece[..], piece[..]);
    }
}
