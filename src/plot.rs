#![allow(dead_code)]

use super::*;
use crate::Piece;
use crate::PIECE_SIZE;
use async_std::fs::File;
use async_std::fs::OpenOptions;
use async_std::io::prelude::*;
use async_std::path::PathBuf;
use async_std::task;
use futures::channel::mpsc;
use futures::channel::mpsc::UnboundedSender;
use futures::channel::oneshot;
use futures::lock::Mutex;
use futures::StreamExt;
use log::error;
use solver::Solution;
use std::collections::HashMap;
use std::convert::TryInto;
use std::io;
use std::io::SeekFrom;
use std::mem;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

const INDEX_LENGTH: usize = mem::size_of::<usize>();
const OFFSET_LENGTH: usize = mem::size_of::<u64>();

#[derive(Debug)]
pub enum PlotCreationError {
    PlotOpen(io::Error),
    PlotMapOpen(io::Error),
    MapRead(io::Error),
}

struct ReadRequest {
    index: usize,
    result_sender: oneshot::Sender<io::Result<Piece>>,
}

struct WriteRequest {
    index: usize,
    piece: oneshot::Sender<Piece>,
    result_sender: oneshot::Sender<io::Result<()>>,
}

/* ToDo
 *
 * Return result for solve()
 * Detect if plot exists on startup and load
 * Delete entire plot (perhaps with script) for testing
 * Extend tests
 * Resize plot by removing the last x indices and adjusting struct params
*/

// TODO: Replace some of the mutexes with more efficient construction
// TODO: There is no synchronization between `map` and `plot_file` for reads, so it is possible to
//  read incorrect data
pub struct Plot {
    map: Mutex<HashMap<usize, u64>>,
    map_file: Mutex<File>,
    plot_file: Mutex<File>,
    read_requests_sender: UnboundedSender<ReadRequest>,
    write_requests_sender: UnboundedSender<WriteRequest>,
    updates: AtomicUsize,
    update_interval: usize,
}

impl Plot {
    /// Creates a new plot for persisting encoded pieces to disk
    pub async fn open_or_create(path: &PathBuf) -> Result<Plot, PlotCreationError> {
        let plot_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path.join("plot.bin"))
            .await
            .map_err(PlotCreationError::PlotOpen)?;

        let mut map_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path.join("plot-map.bin"))
            .await
            .map_err(PlotCreationError::PlotMapOpen)?;

        let mut map = HashMap::new();

        {
            let map_bytes_len = map_file
                .metadata()
                .await
                .map_err(PlotCreationError::MapRead)?
                .len();
            let mut buffer = [0u8; INDEX_LENGTH + OFFSET_LENGTH];
            // let mut buffer = Vec::with_capacity(index_length + offset_length);
            if map_bytes_len > 0 {
                map_file
                    .seek(SeekFrom::Start(0))
                    .await
                    .map_err(PlotCreationError::MapRead)?;
                for _ in (0..map_bytes_len).step_by(INDEX_LENGTH + OFFSET_LENGTH) {
                    if map_file.read_exact(&mut buffer).await.is_err() {
                        error!("Bad map, ignoring remaining bytes");
                        break;
                    }
                    let index =
                        usize::from_le_bytes(buffer[..INDEX_LENGTH].as_ref().try_into().unwrap());
                    let offset =
                        u64::from_le_bytes(buffer[INDEX_LENGTH..].as_ref().try_into().unwrap());
                    map.insert(index, offset);
                }
            }
        }

        let (read_requests_sender, mut read_requests_receiver) = mpsc::unbounded();
        let (write_requests_sender, mut write_requests_receiver) = mpsc::unbounded();

        task::spawn(async move {
            loop {
                // Process as many read requests as there is
                while let Some(read_request) = read_requests_receiver.next().await {
                    todo!("Handle read requests");
                }
                // Process at most write request since reading is higher priority
                if let Some(write_request) = write_requests_receiver.next().await {
                    todo!("Handle write requests");
                }
            }
        });

        let map = Mutex::new(map);
        let map_file = Mutex::new(map_file);
        let plot_file = Mutex::new(plot_file);
        let updates = AtomicUsize::new(0);
        let update_interval = crate::PLOT_UPDATE_INTERVAL;

        Ok(Plot {
            map,
            map_file,
            plot_file,
            read_requests_sender,
            write_requests_sender,
            updates,
            update_interval,
        })
    }

    pub async fn is_empty(&self) -> bool {
        self.map.lock().await.is_empty()
    }

    /// Reads a piece from plot by index
    pub async fn read(&self, index: usize) -> io::Result<Piece> {
        let position = match self.map.lock().await.get(&index) {
            Some(position) => *position,
            None => {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
        };
        self.plot_file
            .lock()
            .await
            .seek(SeekFrom::Start(position))
            .await?;
        let mut buffer = [0u8; PIECE_SIZE];
        self.plot_file.lock().await.read_exact(&mut buffer).await?;
        Ok(buffer)
    }

    /// Writes a piece to the plot by index, will overwrite if piece exists (updates)
    pub async fn write(&self, encoding: &Piece, index: usize) -> io::Result<()> {
        {
            let mut plot_file = self.plot_file.lock().await;

            self.map.lock().await.remove(&index);

            let position = plot_file.seek(SeekFrom::Current(0)).await?;
            plot_file.write_all(&encoding[0..PIECE_SIZE]).await?;

            self.map.lock().await.insert(index, position);
        }
        self.handle_update().await
    }

    /// Removes a piece from the plot by index, by deleting its index from the map
    pub async fn remove(&self, index: usize) -> io::Result<()> {
        self.map.lock().await.remove(&index);
        self.handle_update().await
    }

    /// Fetches the encoding for an audit and returns the solution with random delay
    ///
    /// Given the target:
    /// Given an expected replication factor (encoding_count) as u32
    /// Compute the target value as 2^ (32 - log(2) encoding count)
    ///
    /// Given a sample:
    /// Given a 256 bit tag
    /// Reduce it to a 32 bit number by taking the first four bytes
    /// Convert to an u32 -> f64 -> take log(2)
    /// Compute exponent as log2(tag) - log2(tgt)
    ///
    /// Compute delay as base_delay * 2^exponent
    ///
    pub async fn solve(
        &self,
        challenge: [u8; 32],
        timestamp: u128,
        piece_count: usize,
        replication_factor: u32,
        target: u32,
    ) -> Vec<Solution> {
        // choose the correct "virtual" piece
        let base_index = utils::modulo(&challenge, piece_count);
        let mut solutions: Vec<Solution> = Vec::new();
        // read each "virtual" encoding of that piece
        for i in 0..replication_factor {
            let index = base_index + (i * replication_factor) as usize;
            let encoding = self.read(index).await.unwrap();
            let tag = crypto::create_hmac(&encoding[..], &challenge);
            let sample = utils::bytes_le_to_u32(&tag[0..4]);
            let distance = (sample as f64).log2() - (target as f64).log2();
            let delay = (TARGET_BLOCK_DELAY * 2f64.powf(distance)) as u32;

            solutions.push(Solution {
                challenge,
                base_time: timestamp,
                index: index as u64,
                tag,
                delay,
                encoding,
            })
        }

        // sort the solutions so that smallest delay is first
        solutions.sort_by_key(|s| s.delay);
        solutions[0..DEGREE_OF_SIMULATION].to_vec()
    }

    /// Writes the map to disk to persist between sessions (does not load on startup yet)
    pub async fn force_write_map(&self) -> io::Result<()> {
        // TODO: Writing everything every time is probably not the smartest idea
        let mut map_file = self.map_file.lock().await;
        map_file.seek(SeekFrom::Start(0)).await?;
        map_file
            .set_len(((INDEX_LENGTH + OFFSET_LENGTH) * self.map.lock().await.len()) as u64)
            .await?;
        for (index, offset) in self.map.lock().await.iter() {
            map_file.write_all(&index.to_le_bytes()).await?;
            map_file.write_all(&offset.to_le_bytes()).await?;
        }

        Ok(())
    }

    /// Increment a counter to persist the map based on some interval
    async fn handle_update(&self) -> io::Result<()> {
        let updates = self.updates.fetch_add(1, Ordering::Relaxed);

        if updates % self.update_interval == 0 {
            self.force_write_map().await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;
    use async_std::path::PathBuf;

    #[async_std::test]
    async fn test_basic() {
        let path = PathBuf::from("target").join("test");

        let piece = crypto::generate_random_piece();

        let mut plot = Plot::open_or_create(&path).await.unwrap();
        plot.write(&piece, 0).await.unwrap();
        let extracted_piece = plot.read(0).await.unwrap();

        assert_eq!(extracted_piece[..], piece[..]);

        plot.force_write_map().await.unwrap();
    }
}
