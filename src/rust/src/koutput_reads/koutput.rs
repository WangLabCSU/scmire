use std::path::Path;

use aho_corasick::AhoCorasick;
use anyhow::{anyhow, Context, Result};
use bytes::{Bytes, BytesMut};
use crossbeam_channel::{Receiver, Sender};
use indicatif::{ProgressBar, ProgressFinish};
use memchr::memchr;
use rustc_hash::FxHashMap as HashMap;
use rustc_hash::FxHashSet as HashSet;

use crate::batchsender::BatchSender;
use crate::reader::LineReader;
use crate::utils::*;

pub(super) fn parse_koutput<P: AsRef<Path> + ?Sized>(
    input_path: &P,
    include_sets: HashSet<&[u8]>,
    exclude_aho: Option<AhoCorasick>,
    batch_size: usize,
    nqueue: Option<usize>,
    threads: usize,
) -> Result<HashMap<Bytes, (Bytes, Bytes, Bytes)>> {
    let input: &Path = input_path.as_ref();
    let style = progress_reader_style()?;
    let pb = ProgressBar::new(input.metadata()?.len() as u64).with_finish(ProgressFinish::Abandon);
    pb.set_prefix("Parsing koutput");
    pb.set_style(style);

    // for kmer, we counts total and unique k-mers per taxon across cell barcodes,
    // using both the cell barcode and unique molecular identifier (UMI) to resolve
    // read identity at the single-cell level. It aggregates k-mer counts for each
    // taxonomic rank of interest (by default, genus and species), including all
    // descendant taxa within those ranks.
    std::thread::scope(|scope| {
        // Create a channel between the parser and writer threads
        // The channel transmits batches
        let (koutput_tx, koutput_rx): (
            Sender<Vec<(Bytes, (Bytes, Bytes, Bytes))>>,
            Receiver<Vec<(Bytes, (Bytes, Bytes, Bytes))>>,
        ) = new_channel(None);
        let (reader_tx, reader_rx): (Sender<Vec<BytesMut>>, Receiver<Vec<BytesMut>>) =
            new_channel(nqueue);

        // ─── Parser Thread ─────────────────────────────────────
        // Streams Kraken2 output data, filters by ID set
        let mut parser_handles = Vec::with_capacity(threads);
        for _ in 0 .. threads {
            let rx = reader_rx.clone();
            let tx = koutput_tx.clone();
            let include_sets = &include_sets;
            let exclude_aho = &exclude_aho;
            let handle = scope.spawn(move || -> Result<()> {
                let mut thread_tx = BatchSender::with_capacity(batch_size, tx);
                // let mut compressor = Compressor::new(compression_level);
                while let Ok(lines) = rx.recv() {
                    'chunk_loop: for line in lines {
                        let line = line.freeze();
                        let mut field_start = 0usize;
                        let mut field_index = 0usize;
                        let mut sequence_id = None;
                        let mut taxid = None;
                        let lca;
                        while let Some(tab_pos) = memchr(b'\t', &line[field_start ..]) {
                            let field = &line[field_start .. (field_start + tab_pos)];
                            if field_index == 0 {
                                // Field 0: "C" or "U" — skip unclassified reads
                                if field.len() != 1 || field[0] != b'C' {
                                    continue 'chunk_loop;
                                }
                            } else if field_index == 1 {
                                // Save sequence_id field (field 2)
                                sequence_id = Some(field);
                            } else if field_index == 2 {
                                // Save taxid field (field 3) if it passes filtering
                                // Note: Through the use of `kraken2 --use-names`, 
                                // Kraken 2 will replace the taxonomy ID column 
                                // with the scientific name and the taxonomy ID in 
                                // parenthesis (e.g., "Bacteria (taxid 2)" instead of "2") 
                                if let Some(start) = KOUTPUT_TAXID_PREFIX_FINDER.find(field) {
                                    let start = start + KOUTPUT_TAXID_PREFIX.len();
                                    if let Some(end) = memchr(KOUTPUT_TAXID_SUFFIX, &field[start ..]) {
                                        let id = &field[start .. start + end];
                                        // Skip this line if taxid is not in `include_aho`
                                        if include_sets.contains(id) {
                                            taxid = Some(id);
                                        } else {
                                            continue 'chunk_loop;
                                        };
                                    } else {
                                        continue 'chunk_loop;
                                    };
                                } else if include_sets.contains(field) {
                                    taxid = Some(field);
                                } else {
                                    // Skip line if taxid doesn't contain the prefix
                                    continue 'chunk_loop;
                                }
                            } else if field_index == 3 {
                                // Field 4 (LCA): remainder after the last tab
                                field_start += tab_pos + 1;
                                if let Some(pos) = memchr(b'\t', &line[field_start ..]) {
                                    lca = &line[field_start .. (field_start + pos)]
                                } else {
                                    lca = &line[field_start ..]
                                };
                                if let Some(ref exclude_matcher) = exclude_aho {
                                    if exclude_matcher.find(lca).is_some() {
                                        continue 'chunk_loop;
                                    }
                                }
                                if let (Some(sequence_id), Some(taxid)) = (sequence_id, taxid) {
                                    // Although we *could* use `line.slice_ref()` to avoid extra allocations (by just increasing
                                    // the reference count), we choose `Bytes::copy_from_slice()` to reduce memory usage, as
                                    // the full line buffer may be larger than the selected fields we need to retain.
                                    thread_tx
                                        .send((
                                            Bytes::copy_from_slice(sequence_id),
                                            (
                                                Bytes::copy_from_slice(field), // sequence length
                                                Bytes::copy_from_slice(taxid),
                                                Bytes::copy_from_slice(lca),
                                            ),
                                        ))
                                        .with_context(|| {
                                            format!("(Parser) Failed to send parsed lines to Writer thread")
                                        })?;
                                };
                                continue 'chunk_loop;
                            }
                            field_index += 1;
                            field_start += tab_pos + 1;
                        }
                    }
                }
                thread_tx.flush().with_context(|| {
                    format!("(Parser) Failed to flush parsed lines to Writer thread")
                })?;
                Ok(())
            });
            parser_handles.push(handle);
        }
        drop(reader_rx);
        drop(koutput_tx);

        // ─── reader Thread ─────────────────────────────────────
        let reader_handle = scope.spawn(move || -> Result<()> {
            let mut reader =
                LineReader::with_capacity(BUFFER_SIZE, new_reader(input, BUFFER_SIZE, Some(pb))?);
            let mut reader_tx = BatchSender::with_capacity(batch_size, reader_tx);
            while let Some(record) = reader
                .read_line()
                .with_context(|| format!("(Reader) Failed to read line"))?
            {
                reader_tx
                    .send(record)
                    .with_context(|| format!("(Reader) Failed to send lines to Parser thread"))?;
            }
            reader_tx
                .flush()
                .with_context(|| format!("(Reader) Failed to flush lines to Parser thread"))?;
            Ok(())
        });

        // ─── Join Threads and Propagate Errors ────────────────
        for handler in parser_handles {
            handler
                .join()
                .map_err(|e| anyhow!("(Parser) thread panicked: {:?}", e))??;
        }
        reader_handle
            .join()
            .map_err(|e| anyhow!("(Reader) thread panicked: {:?}", e))??;
        Ok(koutput_rx
            .into_iter()
            .flatten()
            .collect::<HashMap<Bytes, (Bytes, Bytes, Bytes)>>())
    })
}
