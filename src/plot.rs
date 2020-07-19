#![allow(dead_code)]

use crate::{Piece, PIECE_SIZE};
use async_std::fs::File;
use async_std::fs::OpenOptions;
use async_std::io::prelude::*;
use async_std::path::Path;
use std::collections::HashMap;
use std::io::prelude::*;
use std::io::SeekFrom;

/*
 *   Add tests
 *   Init plot as singe file of plot size
 *   Retain plot between runs
 *   Store plot index to disk
 *
*/

pub struct Plot {
    size: usize,
    file: File,
    map: HashMap<usize, u64>,
}

impl Plot {
    pub async fn new(path: &Path, size: usize) -> Plot {
        let parent = path.parent().expect("Can't find parent directory for plot");
        if !parent.exists().await {
            async_std::fs::create_dir_all(parent)
                .await
                .expect("Failed to create directory for plot");
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .await
            .expect("Unable to open");

        // file.set_len(size as u64).unwrap();

        let map: HashMap<usize, u64> = HashMap::new();

        Plot { size, file, map }
    }

    pub async fn read(&mut self, index: usize) -> Piece {
        let position = self.map.get(&index).unwrap();
        self.file.seek(SeekFrom::Start(*position)).await.unwrap();
        let mut buffer = [0u8; PIECE_SIZE];
        self.file.read_exact(&mut buffer).await.unwrap();
        buffer
    }

    pub async fn write(&mut self, encoding: &Piece, index: usize) {
        let position = self.file.seek(SeekFrom::Current(0)).await.unwrap();
        self.file.write_all(&encoding[0..PIECE_SIZE]).await.unwrap();
        self.map.insert(index, position);
    }

    pub async fn remove(&mut self, index: usize) {
        self.map.remove(&index);
    }
}
