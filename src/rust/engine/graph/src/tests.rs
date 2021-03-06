use parking_lot;
use rand;

use std::cmp;
use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use boxfuture::{BoxFuture, Boxable};
use futures01::future::{self, Future};
use hashing::Digest;
use parking_lot::Mutex;

use rand::Rng;

use crate::{EntryId, Graph, InvalidationResult, Node, NodeContext, NodeError};

#[test]
fn create() {
  let graph = Arc::new(Graph::new());
  let context = TContext::new(0, graph.clone());
  assert_eq!(
    graph.create(TNode(2), &context).wait(),
    Ok(vec![T(0, 0), T(1, 0), T(2, 0)])
  );
}

#[test]
fn invalidate_and_clean() {
  let graph = Arc::new(Graph::new());
  let context = TContext::new(0, graph.clone());

  // Create three nodes.
  assert_eq!(
    graph.create(TNode(2), &context).wait(),
    Ok(vec![T(0, 0), T(1, 0), T(2, 0)])
  );
  assert_eq!(context.runs(), vec![TNode(2), TNode(1), TNode(0)]);

  // Clear the middle Node, which dirties the upper node.
  assert_eq!(
    graph.invalidate_from_roots(|&TNode(n)| n == 1),
    InvalidationResult {
      cleared: 1,
      dirtied: 1
    }
  );

  // Confirm that the cleared Node re-runs, and the upper node is cleaned without re-running.
  assert_eq!(
    graph.create(TNode(2), &context).wait(),
    Ok(vec![T(0, 0), T(1, 0), T(2, 0)])
  );
  assert_eq!(context.runs(), vec![TNode(2), TNode(1), TNode(0), TNode(1)]);
}

#[test]
fn invalidate_and_rerun() {
  let graph = Arc::new(Graph::new());
  let context0 = TContext::new(0, graph.clone());

  // Create three nodes.
  assert_eq!(
    graph.create(TNode(2), &context0).wait(),
    Ok(vec![T(0, 0), T(1, 0), T(2, 0)])
  );
  assert_eq!(context0.runs(), vec![TNode(2), TNode(1), TNode(0)]);

  // Clear the middle Node, which dirties the upper node.
  assert_eq!(
    graph.invalidate_from_roots(|&TNode(n)| n == 1),
    InvalidationResult {
      cleared: 1,
      dirtied: 1
    }
  );

  // Request with a new context, which will cause both the middle and upper nodes to rerun since
  // their input values have changed.
  let context1 = TContext::new(1, graph.clone());
  assert_eq!(
    graph.create(TNode(2), &context1).wait(),
    Ok(vec![T(0, 0), T(1, 1), T(2, 1)])
  );
  assert_eq!(context1.runs(), vec![TNode(1), TNode(2)]);
}

#[test]
fn invalidate_with_changed_dependencies() {
  let graph = Arc::new(Graph::new());
  let context = TContext::new(0, graph.clone());

  // Create three nodes.
  assert_eq!(
    graph.create(TNode(2), &context).wait(),
    Ok(vec![T(0, 0), T(1, 0), T(2, 0)])
  );

  // Clear the middle Node, which dirties the upper node.
  assert_eq!(
    graph.invalidate_from_roots(|&TNode(n)| n == 1),
    InvalidationResult {
      cleared: 1,
      dirtied: 1
    }
  );

  // Request with a new context that truncates execution at the middle Node.
  let context = TContext::new_with_dependencies(
    0,
    vec![(TNode(1), None)].into_iter().collect(),
    graph.clone(),
  );
  assert_eq!(
    graph.create(TNode(2), &context).wait(),
    Ok(vec![T(1, 0), T(2, 0)])
  );

  // Confirm that dirtying the bottom Node does not affect the middle/upper Nodes, which no
  // longer depend on it.
  assert_eq!(
    graph.invalidate_from_roots(|&TNode(n)| n == 0),
    InvalidationResult {
      cleared: 1,
      dirtied: 0,
    }
  );
}

#[test]
fn invalidate_randomly() {
  let graph = Arc::new(Graph::new());

  let invalidations = 10;
  let sleep_per_invalidation = Duration::from_millis(100);
  let range = 100;

  // Spawn a background thread to randomly invalidate in the relevant range. Hold its handle so
  // it doesn't detach.
  let graph2 = graph.clone();
  let (send, recv) = mpsc::channel();
  let _join = thread::spawn(move || {
    let mut rng = rand::thread_rng();
    let mut invalidations = invalidations;
    while invalidations > 0 {
      invalidations -= 1;

      // Invalidate a random node in the graph.
      let candidate = rng.gen_range(0, range);
      graph2.invalidate_from_roots(|&TNode(n)| n == candidate);

      thread::sleep(sleep_per_invalidation);
    }
    send.send(()).unwrap();
  });

  // Continuously re-request the root with increasing context values, and assert that Node and
  // context values are ascending.
  let mut iterations = 0;
  let mut max_distinct_context_values = 0;
  loop {
    let context = TContext::new(iterations, graph.clone());

    // Compute the root, and validate its output.
    let node_output = match graph.create(TNode(range), &context).wait() {
      Ok(output) => output,
      Err(TError::Invalidated) => {
        // Some amnount of concurrent invalidation is expected: retry.
        continue;
      }
      Err(e) => panic!(
        "Did not expect any errors other than Invalidation. Got: {:?}",
        e
      ),
    };
    max_distinct_context_values = cmp::max(
      max_distinct_context_values,
      TNode::validate(&node_output).unwrap(),
    );

    // Poll the channel to see whether the background thread has exited.
    if let Ok(_) = recv.try_recv() {
      break;
    }
    iterations += 1;
  }

  assert!(
    max_distinct_context_values > 1,
    "In {} iterations, observed a maximum of {} distinct context values.",
    iterations,
    max_distinct_context_values
  );
}

#[test]
fn drain_and_resume() {
  // Confirms that after draining a Graph that has running work, we are able to resume the work
  // and have it complete successfully.
  let graph = Arc::new(Graph::new());

  let delay_before_drain = Duration::from_millis(100);
  let delay_in_task = delay_before_drain * 10;

  // Create a context that will sleep long enough at TNode(1) to be interrupted before
  // requesting TNode(0).
  let context = {
    let mut delays = HashMap::new();
    delays.insert(TNode(1), delay_in_task);
    TContext::new_with_delays(0, delays, graph.clone())
  };

  // Spawn a background thread that will mark the Graph draining after a short delay.
  let graph2 = graph.clone();
  let _join = thread::spawn(move || {
    thread::sleep(delay_before_drain);
    graph2
      .mark_draining(true)
      .expect("Should not already be draining.");
  });

  // Request a TNode(1) in the "delayed" context, and expect it to be interrupted by the
  // drain.
  assert_eq!(
    graph.create(TNode(2), &context).wait(),
    Err(TError::Invalidated),
  );

  // Unmark the Graph draining, and try again: we expect the `Invalidated` result we saw before
  // due to the draining to not have been persisted.
  graph
    .mark_draining(false)
    .expect("Should already be draining.");
  assert_eq!(
    graph.create(TNode(2), &context).wait(),
    Ok(vec![T(0, 0), T(1, 0), T(2, 0)])
  );
}

#[test]
fn cyclic_failure() {
  // Confirms that an attempt to create a cycle fails.
  let graph = Arc::new(Graph::new());
  let top = TNode(2);
  let context = TContext::new_with_dependencies(
    0,
    // Request creation of a cycle by sending the bottom most node to the top.
    vec![(TNode(0), Some(top))].into_iter().collect(),
    graph.clone(),
  );

  assert_eq!(graph.create(TNode(2), &context).wait(), Err(TError::Cyclic));
}

#[test]
fn cyclic_dirtying() {
  // Confirms that a dirtied path between two nodes is able to reverse direction while being
  // cleaned.
  let graph = Arc::new(Graph::new());
  let initial_top = TNode(2);
  let initial_bot = TNode(0);

  // Request with a context that creates a path downward.
  let context_down = TContext::new(0, graph.clone());
  assert_eq!(
    graph.create(initial_top.clone(), &context_down).wait(),
    Ok(vec![T(0, 0), T(1, 0), T(2, 0)])
  );

  // Clear the bottom node, and then clean it with a context that causes the path to reverse.
  graph.invalidate_from_roots(|n| n == &initial_bot);
  let context_up = TContext::new_with_dependencies(
    1,
    // Reverse the path from bottom to top.
    vec![(TNode(1), None), (TNode(0), Some(TNode(1)))]
      .into_iter()
      .collect(),
    graph.clone(),
  );

  let res = graph.create(initial_bot, &context_up).wait();

  assert_eq!(res, Ok(vec![T(1, 1), T(0, 1)]));

  let res = graph.create(initial_top, &context_up).wait();

  assert_eq!(res, Ok(vec![T(1, 1), T(2, 1)]));
}

#[test]
fn critical_path() {
  use super::entry::Entry;
  // First, let's describe the scenario with plain data.
  //
  // We label the nodes with static strings to help visualise the situation.
  // The first element of each tuple is a readable label. The second element represents the
  // duration for this action.
  let nodes = [
    ("download jvm", 10),
    ("download a", 1),
    ("download b", 2),
    ("download c", 3),
    ("compile a", 3),
    ("compile b", 20),
    ("compile c", 5),
  ];
  let deps = [
    ("download jvm", "compile a"),
    ("download jvm", "compile b"),
    ("download jvm", "compile c"),
    ("download a", "compile a"),
    ("download b", "compile b"),
    ("download c", "compile c"),
    ("compile a", "compile c"),
    ("compile b", "compile c"),
  ];

  // Describe a few transformations to navigate between our readable data and the actual types
  // needed for the graph.
  let tnode = |node: &str| {
    TNode(
      nodes
        .iter()
        .map(|(k, _)| k)
        .position(|label| &node == label)
        .unwrap(),
    )
  };
  let node_key = |node: &str| tnode(node);
  let node_entry = |node: &str| Entry::new(node_key(node));
  let node_and_duration_from_entry = |entry: &super::entry::Entry<TNode>| nodes[entry.node().0];
  let node_duration =
    |entry: &super::entry::Entry<TNode>| Duration::from_secs(node_and_duration_from_entry(entry).1);

  // Construct a graph and populate it with the nodes and edges prettily defined above.
  let graph = Graph::new();
  {
    let inner = &mut graph.inner.lock();
    for (node, _) in &nodes {
      let node_index = inner.pg.add_node(node_entry(node));
      inner.nodes.insert(node_key(node), node_index);
    }
    for (src, dst) in &deps {
      let src = inner.nodes[&node_key(src)];
      let dst = inner.nodes[&node_key(dst)];
      inner.pg.add_edge(src, dst, 1.0);
    }
  }

  // Calculate the critical path and validate it.
  {
    // The roots are all the sources, so we're covering the entire graph
    let roots = ["download jvm", "download a", "download b", "download c"]
      .iter()
      .map(|n| tnode(n))
      .collect::<Vec<_>>();
    let (expected_total_duration, expected_critical_path) = (
      Duration::from_secs(35),
      vec!["download jvm", "compile b", "compile c"],
    );
    let (total_duration, critical_path) = graph.critical_path(&roots, &node_duration);
    assert_eq!(expected_total_duration, total_duration);
    let critical_path = critical_path
      .iter()
      .map(|entry| node_and_duration_from_entry(entry).0)
      .collect::<Vec<_>>();
    assert_eq!(expected_critical_path, critical_path);
  }
  {
    // The roots exclude some nodes ("download jvm", "download a") from the graph.
    let roots = ["download b", "download c"]
      .iter()
      .map(|n| tnode(n))
      .collect::<Vec<_>>();
    let (expected_total_duration, expected_critical_path) = (
      Duration::from_secs(27),
      vec!["download b", "compile b", "compile c"],
    );
    let (total_duration, critical_path) = graph.critical_path(&roots, &node_duration);
    assert_eq!(expected_total_duration, total_duration);
    let critical_path = critical_path
      .iter()
      .map(|entry| node_and_duration_from_entry(entry).0)
      .collect::<Vec<_>>();
    assert_eq!(expected_critical_path, critical_path);
  }
}

///
/// A token containing the id of a Node and the id of a Context, respectively. Has a short name
/// to minimize the verbosity of tests.
///
#[derive(Clone, Debug, Eq, PartialEq)]
struct T(usize, usize);

///
/// A node that builds a Vec of tokens by recursively requesting itself and appending its value
/// to the result.
///
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TNode(usize);
impl Node for TNode {
  type Context = TContext;
  type Item = Vec<T>;
  type Error = TError;

  fn run(self, context: TContext) -> BoxFuture<Vec<T>, TError> {
    context.ran(self.clone());
    let token = T(self.0, context.id());
    if let Some(dep) = context.dependency_of(&self) {
      context.maybe_delay(&self);
      context
        .get(dep)
        .map(move |mut v| {
          v.push(token);
          v
        })
        .to_boxed()
    } else {
      future::ok(vec![token]).to_boxed()
    }
  }

  fn digest(_result: Self::Item) -> Option<Digest> {
    None
  }

  fn cacheable(&self) -> bool {
    true
  }
}

impl std::fmt::Display for TNode {
  fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
    write!(f, "{:?}", self)
  }
}

impl TNode {
  ///
  /// Validates the given TNode output. Both node ids and context ids should increase left to
  /// right: node ids monotonically, and context ids non-monotonically.
  ///
  /// Valid:
  ///   (0,0), (1,1), (2,2), (3,3)
  ///   (0,0), (1,0), (2,1), (3,1)
  ///
  /// Invalid:
  ///   (0,0), (1,1), (2,1), (3,0)
  ///   (0,0), (1,0), (2,0), (1,0)
  ///
  /// If successful, returns the count of distinct context ids in the path.
  ///
  fn validate(output: &Vec<T>) -> Result<usize, String> {
    let (node_ids, context_ids): (Vec<_>, Vec<_>) = output
      .iter()
      .map(|&T(node_id, context_id)| {
        // We cast to isize to allow comparison to -1.
        (node_id as isize, context_id)
      })
      .unzip();
    // Confirm monotonically ordered.
    let mut previous: isize = -1;
    for node_id in node_ids {
      if previous + 1 != node_id {
        return Err(format!(
          "Node ids in {:?} were not monotonically ordered.",
          output
        ));
      }
      previous = node_id;
    }
    // Confirm ordered (non-monotonically).
    let mut previous: usize = 0;
    for &context_id in &context_ids {
      if previous > context_id {
        return Err(format!("Context ids in {:?} were not ordered.", output));
      }
      previous = context_id;
    }

    Ok(context_ids.into_iter().collect::<HashSet<_>>().len())
  }
}

///
/// A context that keeps a record of Nodes that have been run.
///
#[derive(Clone)]
struct TContext {
  id: usize,
  // A mapping from source to optional destination that drives what values each TNode depends on.
  // If there is no entry in this map for a node, then TNode::run will default to requesting
  // the next smallest node. Finally, if a None entry is present, a node will have no
  // dependencies.
  edges: Arc<HashMap<TNode, Option<TNode>>>,
  delays: HashMap<TNode, Duration>,
  graph: Arc<Graph<TNode>>,
  runs: Arc<Mutex<Vec<TNode>>>,
  entry_id: Option<EntryId>,
}
impl NodeContext for TContext {
  type Node = TNode;
  fn clone_for(&self, entry_id: EntryId) -> TContext {
    TContext {
      id: self.id,
      edges: self.edges.clone(),
      delays: self.delays.clone(),
      graph: self.graph.clone(),
      runs: self.runs.clone(),
      entry_id: Some(entry_id),
    }
  }

  fn graph(&self) -> &Graph<TNode> {
    &self.graph
  }

  fn spawn<F>(&self, future: F)
  where
    F: Future<Item = (), Error = ()> + Send + 'static,
  {
    // Avoids introducing a dependency on a threadpool.
    thread::spawn(move || {
      future.wait().unwrap();
    });
  }
}

impl TContext {
  fn new(id: usize, graph: Arc<Graph<TNode>>) -> TContext {
    TContext {
      id,
      edges: Arc::default(),
      delays: HashMap::default(),
      graph,
      runs: Arc::new(Mutex::new(Vec::new())),
      entry_id: None,
    }
  }

  fn new_with_dependencies(
    id: usize,
    edges: HashMap<TNode, Option<TNode>>,
    graph: Arc<Graph<TNode>>,
  ) -> TContext {
    TContext {
      id,
      edges: Arc::new(edges),
      delays: HashMap::default(),
      graph,
      runs: Arc::new(Mutex::new(Vec::new())),
      entry_id: None,
    }
  }

  fn new_with_delays(
    id: usize,
    delays: HashMap<TNode, Duration>,
    graph: Arc<Graph<TNode>>,
  ) -> TContext {
    TContext {
      id,
      edges: Arc::default(),
      delays,
      graph,
      runs: Arc::new(Mutex::new(Vec::new())),
      entry_id: None,
    }
  }

  fn id(&self) -> usize {
    self.id
  }

  fn get(&self, dst: TNode) -> BoxFuture<Vec<T>, TError> {
    self.graph.get(self.entry_id.unwrap(), self, dst)
  }

  fn ran(&self, node: TNode) {
    let mut runs = self.runs.lock();
    runs.push(node);
  }

  fn maybe_delay(&self, node: &TNode) {
    if let Some(delay) = self.delays.get(node) {
      thread::sleep(*delay);
    }
  }

  ///
  /// If the given TNode should declare a dependency on another TNode, returns that dependency.
  ///
  fn dependency_of(&self, node: &TNode) -> Option<TNode> {
    match self.edges.get(node) {
      Some(Some(ref dep)) => Some(dep.clone()),
      Some(None) => None,
      None if node.0 > 0 => Some(TNode(node.0 - 1)),
      None => None,
    }
  }

  fn runs(&self) -> Vec<TNode> {
    self.runs.lock().clone()
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TError {
  Cyclic,
  Invalidated,
}
impl NodeError for TError {
  fn invalidated() -> Self {
    TError::Invalidated
  }

  fn cyclic(_path: Vec<String>) -> Self {
    TError::Cyclic
  }
}
