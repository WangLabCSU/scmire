use std::io::BufWriter;
use std::io::Write;
use std::iter::zip;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender};
use indicatif::ProgressBar;
use libdeflater::{CompressionLvl, Compressor};
use rustc_hash::FxHashSet as HashSet;

use crate::batchsender::BatchSender;
use crate::fastq_reader::*;
use crate::fastq_record::{FastqParseError, FastqRecord};
use crate::utils::*;

pub(super) fn parse_paired<P: AsRef<Path> + ?Sized>(
    id_sets: &HashSet<&[u8]>,
    input1_path: &P,
    input1_bar: Option<ProgressBar>,
    input2_path: &P,
    input2_bar: Option<ProgressBar>,
    output1_path: Option<&P>,
    output1_bar: Option<ProgressBar>,
    output2_path: Option<&P>,
    output2_bar: Option<ProgressBar>,
    compression_level: i32,
    batch_size: usize,
    chunk_bytes: usize,
    nqueue: Option<usize>,
    threads: usize,
) -> Result<()> {
    let compression_level = CompressionLvl::new(compression_level)
        .map_err(|e| anyhow!("Invalid 'compression_level': {:?}", e))?;
    std::thread::scope(|scope| -> Result<()> {
        // Create a channel between the parser and writer threads
        // The channel transmits batches (Vec<FastqRecord>)
        let (writer_tx, writer_rx): (
            Sender<(Option<Vec<u8>>, Option<Vec<u8>>)>,
            Receiver<(Option<Vec<u8>>, Option<Vec<u8>>)>,
        ) = new_channel(nqueue);
        let (writer1_tx, writer1_rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = new_channel(nqueue);
        let (writer2_tx, writer2_rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = new_channel(nqueue);

        let (reader_tx, reader_rx): (
            Sender<(Vec<FastqRecord<Bytes>>, Vec<FastqRecord<Bytes>>)>,
            Receiver<(Vec<FastqRecord<Bytes>>, Vec<FastqRecord<Bytes>>)>,
        ) = new_channel(nqueue);
        let (reader1_tx, reader1_rx): (
            Sender<Vec<FastqRecord<Bytes>>>,
            Receiver<Vec<FastqRecord<Bytes>>>,
        ) = new_channel(nqueue);
        let (reader2_tx, reader2_rx): (
            Sender<Vec<FastqRecord<Bytes>>>,
            Receiver<Vec<FastqRecord<Bytes>>>,
        ) = new_channel(nqueue);

        // ─── Writer Thread ─────────────────────────────────────
        let (writer1_handle, gzip1) = if let Some(output_path) = output1_path {
            let output: &Path = output_path.as_ref();
            let handle = Some(scope.spawn(move || -> Result<()> {
                let mut writer =
                    BufWriter::with_capacity(chunk_bytes, new_writer(output, output1_bar)?);
                for chunk in writer1_rx {
                    writer.write_all(&chunk).with_context(|| {
                        format!("(Writer1) Failed to write Fastq records to output")
                    })?;
                }
                writer
                    .flush()
                    .with_context(|| format!("(Writer1) Failed to flush writer"))?;
                Ok(())
            }));
            let gzip = gz_compressed(output);
            (handle, gzip)
        } else {
            (None, false)
        };

        let (writer2_handle, gzip2) = if let Some(output_path) = output2_path {
            let output: &Path = output_path.as_ref();
            let handle = Some(scope.spawn(move || -> Result<()> {
                let mut writer =
                    BufWriter::with_capacity(chunk_bytes, new_writer(output, output2_bar)?);
                for chunk in writer2_rx {
                    writer.write_all(&chunk).with_context(|| {
                        format!("(Writer2) Failed to write Fastq records to output")
                    })?;
                }
                writer
                    .flush()
                    .with_context(|| format!("(Writer2) Failed to flush writer"))?;
                Ok(())
            }));
            let gzip = gz_compressed(output);
            (handle, gzip)
        } else {
            (None, false)
        };

        // Consumes batches of records and writes them to file
        let writer_handle = scope.spawn(move || -> Result<()> {
            // Iterate over each received batch of records
            for (records1, records2) in writer_rx {
                if let Some(records1) = records1 {
                    writer1_tx.send(records1).with_context(|| {
                        format!("(Writer dispatch) Failed to send read1 batch to Writer1 thread")
                    })?;
                }
                if let Some(records2) = records2 {
                    writer2_tx.send(records2).with_context(|| {
                        format!("(Writer dispatch) Failed to send read2 batch to Writer2 thread")
                    })?;
                }
            }
            Ok(())
        });

        // ─── Parser Thread ─────────────────────────────────────
        let has_writer1 = writer1_handle.is_some();
        let has_writer2 = writer2_handle.is_some();
        let mut parser_handles = Vec::with_capacity(threads);
        for _ in 0 .. threads {
            let rx = reader_rx.clone();
            let tx = writer_tx.clone();
            let handle = scope.spawn(move || -> Result<()> {
                let mut records1_pool: Vec<u8> = Vec::with_capacity(chunk_bytes);
                let mut records2_pool: Vec<u8> = Vec::with_capacity(chunk_bytes);
                let mut compressor = Compressor::new(compression_level);
                while let Ok((records1, records2)) = rx.recv() {
                    // Initialize a thread-local batch sender for matching records
                    for (record1, record2) in zip(records1, records2) {
                        if record1.id != record2.id {
                            return Err(
                                anyhow!("{}", FastqParseError::FastqPairError { read1_id: String::from_utf8_lossy(&record1.id).to_string(), read2_id: String::from_utf8_lossy(&record2.id).to_string(), read1_pos: None, read2_pos: None }
                            ));
                        }
                        if id_sets.contains(record1.id.as_ref()) {
                        if records1_pool.capacity() - records1_pool.len() < record1.bytes_size() ||
                            records2_pool.capacity() - records2_pool.len() < record2.bytes_size() {
                            let pack1 = if has_writer1 {
                                let mut pack = Vec::with_capacity(chunk_bytes);
                                std::mem::swap(&mut records1_pool, &mut pack);
                                if gzip1 {
                                    pack = gzip_pack(&pack, &mut compressor)?
                                }
                                Some(pack)
                            } else {
                                None
                            };
                            let pack2 = if has_writer2 {
                                let mut pack = Vec::with_capacity(chunk_bytes);
                                std::mem::swap(&mut records2_pool, &mut pack);
                                if gzip2 {
                                    pack = gzip_pack(&pack, &mut compressor)?
                                }
                                Some(pack)
                            } else {
                                None
                            };
                            tx.send((pack1, pack2)).with_context(|| {
                                format!(
                                    "(Parser) Failed to send send parsed record pair to Writer thread"
                                )
                            })?;
                        }
                        record1.extend(&mut records1_pool);
                        record2.extend(&mut records2_pool);
                    }
                    }
                }
                if !records1_pool.is_empty() {
                    let pack1 = if has_writer1 {
                        let pack = if gzip1 {
                            gzip_pack(&records1_pool, &mut compressor)?
                        } else {
                            records1_pool
                        };
                        Some(pack)
                    } else {
                        None
                    };
                    let pack2 = if has_writer2 {
                        let pack = if gzip2 {
                            gzip_pack(&records2_pool, &mut compressor)?
                        } else {
                            records2_pool
                        };
                        Some(pack)
                    } else {
                        None
                    };
                    tx.send((pack1, pack2)).with_context(|| {
                        format!(
                            "(Parser) Failed to send send parsed record pair to Writer thread"
                        )
                    })?;
                }
                Ok(())
            });
            parser_handles.push(handle);
        }
        drop(reader_rx);
        drop(writer_tx);

        // ─── reader Thread ─────────────────────────────────────
        let reader_handle = scope.spawn(move || -> Result<()> {
            loop {
                let (records1, records2) = match (reader1_rx.recv(), reader2_rx.recv()) {
                    (Ok(rec1), Ok(rec2)) => (rec1, rec2),
                    (Err(_), Ok(_)) => {
                        return Err(anyhow!(
                            "(Reader collect) FASTQ pairing error: read1 channel closed before read2"
                        ));
                    }
                    (Ok(_), Err(_)) => {
                        return Err(anyhow!(
                            "(Reader collect) FASTQ pairing error: read2 channel closed before read1"
                        ));
                    }
                    (Err(_), Err(_)) => {
                        break;
                    }
                };
                if records1.len() != records2.len() {
                    return Err(anyhow!("(Reader collect) FASTQ pairing error: record count mismatch (read1: {}, read2: {})", records1.len(), records2.len()));
                }
                reader_tx.send((records1, records2)).with_context(|| {
                    format!(
                        "(Reader collect) Failed to send send parsed record pair to Parser thread"
                    )
                })?;
            }
            Ok(())
        });

        let input1: &Path = input1_path.as_ref();
        let reader1_handle = scope.spawn(move || -> Result<()> {
            let mut reader = FastqReader::with_capacity(
                BUFFER_SIZE,
                new_reader(input1, BUFFER_SIZE, input1_bar)?,
            );
            let mut thread_tx = BatchSender::with_capacity(batch_size, reader1_tx);
            while let Some(record) = reader
                .read_record()
                .with_context(|| format!("(Reader1) Failed to read FASTQ record"))?
            {
                thread_tx.send(record).with_context(|| {
                    format!("(Reader1) Failed to send FASTQ record to reader collect thread")
                })?;
            }
            thread_tx.flush().with_context(|| {
                format!("(Reader1) Failed to flush records to reader collect thread")
            })?;
            Ok(())
        });

        let input2: &Path = input2_path.as_ref();
        let reader2_handle = scope.spawn(move || -> Result<()> {
            let mut reader = FastqReader::with_capacity(
                BUFFER_SIZE,
                new_reader(input2, BUFFER_SIZE, input2_bar)?,
            );
            let mut thread_tx = BatchSender::with_capacity(batch_size, reader2_tx);
            while let Some(record) = reader
                .read_record()
                .with_context(|| format!("(Reader2) Failed to read FASTQ record"))?
            {
                thread_tx.send(record).with_context(|| {
                    format!("(Reader2) Failed to send FASTQ record to reader collect thread")
                })?;
            }
            thread_tx.flush().with_context(|| {
                format!("(Reader2) Failed to flush records to reader collect thread")
            })?;
            Ok(())
        });

        // ─── Join Threads and Propagate Errors ────────────────
        if let Some(writer_handle) = writer1_handle {
            writer_handle
                .join()
                .map_err(|e| anyhow!("(Writer1) thread panicked: {:?}", e))??;
        };
        if let Some(writer_handle) = writer2_handle {
            writer_handle
                .join()
                .map_err(|e| anyhow!("(Writer2) thread panicked: {:?}", e))??;
        };
        writer_handle
            .join()
            .map_err(|e| anyhow!("(Writer dispatch) thread panicked: {:?}", e))??;

        for handler in parser_handles {
            handler
                .join()
                .map_err(|e| anyhow!("(Parser) thread panicked: {:?}", e))??;
        }
        reader_handle
            .join()
            .map_err(|e| anyhow!("(Reader collect) thread panicked: {:?}", e))??;
        reader1_handle
            .join()
            .map_err(|e| anyhow!("(Reader1) thread panicked: {:?}", e))??;
        reader2_handle
            .join()
            .map_err(|e| anyhow!("(Reader2) thread panicked: {:?}", e))??;
        Ok(())
    })
}
