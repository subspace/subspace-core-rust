#![allow(dead_code)]

use super::*;
use crate::plot::Plot;
use async_std::task;
// use indicatif::ProgressBar;
use async_std::path::PathBuf;
use log::*;
use rayon::prelude::*;
use rug::integer::Order;
use rug::Integer;
use std::sync::Arc;
use std::time::Instant;

/* ToDo
 *
 * -- Functionality --
 *
 *
 * -- Polish --
 * Read drives and free disk space (sysinfo)
 * Accept user input
 * prevent computer from sleeping (enigo)
 *
*/

pub async fn plot(path: PathBuf, node_id: NodeID, genesis_piece: Piece) -> Arc<Plot> {
    // init plot
    let plot = Arc::new(Plot::open_or_create(&path).await.unwrap());

    if plot.is_empty().await {
        let plotting_fut = task::spawn_blocking({
            let plot = Arc::clone(&plot);

            move || {
                let expanded_iv = crypto::expand_iv(node_id);
                let integer_expanded_iv = Integer::from_digits(&expanded_iv, Order::Lsf);
                let piece = genesis_piece;

                // init sloth
                let sloth = sloth::Sloth::init(PRIME_SIZE_BITS);

                // plot pieces in parallel on all cores, using IV as a source of randomness
                // this is just for efficient testing atm
                (0..PLOT_SIZE).into_par_iter().for_each(|index| {
                    let mut piece = piece;

                    // xor first 16 bytes of piece with the index to get a unique piece for each iteration
                    let index_bytes = utils::usize_to_bytes(index);
                    for i in 0..16 {
                        piece[i] = piece[i] ^ index_bytes[i];
                    }

                    sloth
                        .encode(&mut piece, &integer_expanded_iv, ENCODING_LAYERS_TEST)
                        .unwrap();
                    task::block_on(plot.write(&piece, index)).unwrap();
                    // bar.inc(1);
                });
            }
        });

        // let bar = ProgressBar::new(PLOT_SIZE as u64);
        let plot_time = Instant::now();

        info!("Sloth is slowly plotting {} pieces...", PLOT_SIZE);

        plotting_fut.await;

        // bar.finish();

        let total_plot_time = plot_time.elapsed();
        let average_plot_time =
            (total_plot_time.as_nanos() / PLOT_SIZE as u128) as f32 / (1000f32 * 1000f32);

        info!("Average plot time is {:.3} ms per piece", average_plot_time);

        info!(
            "Total plot time is {:.3} minutes",
            total_plot_time.as_secs_f32() / 60f32
        );

        info!(
            "Plotting throughput is {} mb/sec\n",
            ((PLOT_SIZE as u64 * PIECE_SIZE as u64) / (1000 * 1000)) as f32
                / (total_plot_time.as_secs_f32())
        );
    } else {
        info!("Using existing plot...");
    }

    plot
}
