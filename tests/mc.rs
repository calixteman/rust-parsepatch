extern crate parsepatch;
use crate::parsepatch::*;

use crossbeam::channel::Receiver;
use crossbeam::crossbeam_channel::unbounded;
use hglib::{export, hg, runcommand, Client, MkArg, Runner};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::thread;

struct DiffImpl;

struct PatchImpl {
    diffs: Vec<DiffImpl>,
}

impl Patch<DiffImpl> for PatchImpl {
    fn new_diff(&mut self) -> &mut DiffImpl {
        self.diffs.push(DiffImpl {});
        self.diffs.last_mut().unwrap()
    }

    fn close(&mut self) {}
}

impl Diff for DiffImpl {
    fn set_info(
        &mut self,
        _old_name: &str,
        _new_name: &str,
        _op: FileOp,
        _binary_sizes: Option<Vec<BinaryHunk>>,
        _file_mode: Option<FileMode>,
    ) {
    }

    fn add_line(&mut self, _old_line: u32, _new_line: u32, _line: &[u8]) {}
    fn new_hunk(&mut self) {}
    fn close(&mut self) {}
}

fn get_log(path: &str, range: &str) -> Vec<String> {
    let mut client = Client::open(&path, "UTF-8", &[]).unwrap();
    let range = [range];
    let (mut data, _) = runcommand!(
        client,
        "log",
        &[""],
        "--template",
        "{node}\\0",
        "-r",
        &range
    )
    .unwrap();
    data.remove(data.len() - 1);
    data.split(|&c| c == b'\0')
        .map(|s| unsafe { String::from_utf8_unchecked(s.to_vec()) })
        .collect()
}

fn consumer(
    path: String,
    receiver: Receiver<Option<Vec<String>>>,
    set: &Mutex<HashSet<String>>,
    total: usize,
) {
    let mut client = Client::open(&path, "UTF-8", &[]).unwrap();
    let poisons = num_cpus::get();

    while let Ok(nodes) = receiver.recv() {
        if nodes.is_none() {
            break;
        }

        let mut nodes = nodes.unwrap();
        for node in nodes.drain(..) {
            let patch = hg!(client, export, revs = &[&node]).unwrap();

            if let Some(diff) = patch {
                let mut patch = PatchImpl { diffs: Vec::new() };

                {
                    let mut set = set.lock().unwrap();
                    set.insert(node.clone());
                }
                PatchReader::by_buf(&diff, &mut patch).unwrap();
                {
                    let mut set = set.lock().unwrap();
                    set.remove(&node);
                }
            }
        }

        let queue_len = receiver.len();
        if queue_len >= poisons {
            let treated = (total - (queue_len - poisons));
            let percent = treated as f64 / (total as f64) * 100.;
            println!(
                "{:.2}% of the chunks processed ({} / {})",
                percent, treated, total
            );
        }
    }
}

#[test]
fn test_mc() {
    let path = "/home/calixte/dev/mozilla/mozilla-central.hg";
    let mut nodes = get_log(path, "");
    let total = nodes.len();
    let set = Arc::new(Mutex::new(HashSet::new()));
    let n_threads = num_cpus::get();
    let (sender, receiver) = unbounded();
    let chunk_size = 32.min(total / n_threads + 1);

    println!(
        "Chunk size is {} for {} collected nodes.",
        chunk_size, total
    );

    for c in nodes.chunks(chunk_size) {
        sender.send(Some(c.to_vec())).unwrap();
    }

    let total = sender.len();

    let mut threads = Vec::new();
    for i in 0..num_cpus::get() {
        let receiver = receiver.clone();
        let path = path.to_string();
        let set = Arc::clone(&set);

        let t = thread::Builder::new()
            .name(format!("Consumer {}", i))
            .spawn(move || {
                consumer(path, receiver, &set, total);
            })
            .unwrap();

        threads.push(t);
    }

    for _ in 0..num_cpus::get() {
        sender.send(None).unwrap();
    }

    for t in threads.drain(..) {
        if let Err(e) = t.join() {
            eprintln!("Error {:?}", e);
            let set = set.lock().unwrap();
            eprintln!("Current parsed patches:\n{:?}", set);
        }
    }

    let erroneous = Arc::try_unwrap(set).unwrap().into_inner().unwrap();
    assert!(erroneous.is_empty(), "Erroneous patches:\n{:?}", erroneous);
}
