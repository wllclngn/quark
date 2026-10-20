//! The launch as a topological sort. Each node names the nodes it needs;
//! `run` keeps the in-degree of every node, starts every node whose count
//! is zero on its own scoped thread, and as each one finishes releases
//! the nodes that were waiting on it (Kahn's algorithm, with the
//! scheduling loop built in). Outputs are keyed by node name and read by
//! dependents, which always run after their producers.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Instant;

pub(crate) enum Out {
    Done,
    Dlls(Vec<&'static str>),
}

pub(crate) struct Node<'a> {
    pub(crate) name: &'static str,
    pub(crate) needs: &'static [&'static str],
    work: Box<dyn FnOnce(&Outputs) -> Out + Send + 'a>,
}

impl<'a> Node<'a> {
    pub(crate) fn new(
        name: &'static str,
        needs: &'static [&'static str],
        work: impl FnOnce(&Outputs) -> Out + Send + 'a,
    ) -> Self {
        Node { name, needs, work: Box::new(work) }
    }
}

#[derive(Default)]
pub(crate) struct Outputs {
    map: Mutex<HashMap<&'static str, Out>>,
}

impl Outputs {
    pub(crate) fn dlls(&self, name: &'static str) -> Vec<&'static str> {
        match self.map.lock().unwrap().get(name) {
            Some(Out::Dlls(d)) => d.clone(),
            _ => Vec::new(),
        }
    }
}

/// Per-node wall time in milliseconds, in completion order.
pub(crate) type Timings = Vec<(&'static str, u128)>;

pub(crate) fn run(nodes: Vec<Node<'_>>) -> (Outputs, Timings) {
    let outputs = Outputs::default();
    let mut timings: Timings = Vec::new();

    let mut indegree: HashMap<&'static str, usize> = HashMap::new();
    let mut dependents: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
    for n in &nodes {
        indegree.insert(n.name, n.needs.len());
        for need in n.needs {
            dependents.entry(need).or_default().push(n.name);
        }
    }
    let mut pending: HashMap<&'static str, Node<'_>> =
        nodes.into_iter().map(|n| (n.name, n)).collect();
    let total = pending.len();

    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel::<(&'static str, Out, u128)>();
        let mut finished = 0usize;
        loop {
            let ready: Vec<&'static str> = indegree
                .iter()
                .filter(|(name, deg)| **deg == 0 && pending.contains_key(**name))
                .map(|(name, _)| *name)
                .collect();
            for name in ready {
                let node = pending.remove(name).unwrap();
                let tx = tx.clone();
                let outputs = &outputs;
                s.spawn(move || {
                    let t0 = Instant::now();
                    let out = (node.work)(outputs);
                    let _ = tx.send((node.name, out, t0.elapsed().as_millis()));
                });
            }
            if finished == total {
                break;
            }
            let Ok((name, out, ms)) = rx.recv() else { break };
            finished += 1;
            timings.push((name, ms));
            outputs.map.lock().unwrap().insert(name, out);
            for dep in dependents.get(name).map(|v| v.as_slice()).unwrap_or(&[]) {
                if let Some(d) = indegree.get_mut(dep) {
                    *d -= 1;
                }
            }
        }
    });
    (outputs, timings)
}
