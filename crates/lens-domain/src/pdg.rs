//! Program dependence graphs over the [`TreeNode`] body currency, and
//! the graph-kernel similarity `analyze similarity --method pdg` scores
//! them with.
//!
//! A tree-edit distance sees a function as the order its statements were
//! written in. A program dependence graph (PDG) sees it as what each
//! statement needs from the others: one node per statement, a *control*
//! edge from a predicate to every statement it decides the execution of,
//! and a *data* edge from a statement that binds a name to every later
//! statement that reads it. Two bodies that compute the same thing with
//! the statements shuffled, the locals renamed, or an unrelated statement
//! slipped in between have the same graph, which is exactly the clone a
//! syntactic comparison scores low on.
//!
//! The graph is built from the same [`TreeNode`] the other methods
//! compare, so no adapter needs a second lowering. What an adapter does
//! own is its label vocabulary — which node is a read of a local, which
//! construct binds one, which node holds a nested statement list — and
//! it states that once through [`DependenceVocabulary`]. Everything
//! after that (reaching definitions, branch merging, loop back-edges,
//! the kernel) is language-neutral and lives here.
//!
//! Similarity is a normalised Weisfeiler-Lehman subtree kernel over the
//! statement graph, averaged with the overlap of the statements' own
//! canonical fingerprints: the first half asks whether the *wiring* of
//! the two bodies matches, the second whether the *statements* do. See
//! [`pdg_similarity`].

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};

use crate::tree::TreeNode;

/// Rounds of neighbourhood relabelling the kernel runs. Iteration 0 is
/// the bare statement kind; each further round folds in the labels one
/// hop further out, so at two rounds a node's label encodes the shape of
/// the dependence chain two statements around it. Deeper rounds make a
/// single inserted statement disturb more labels than the insertion is
/// worth.
pub const WL_ITERATIONS: usize = 2;

/// What a node means to dependence analysis, as classified by the
/// adapter that emitted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependenceRole<'a> {
    /// Structure with no dependence meaning of its own; its children are
    /// walked.
    Plain,
    /// A read of the named local (a bare identifier expression).
    Reference(&'a str),
    /// A construct that binds `names` for the statements after it. The
    /// children at `targets` are the binding patterns and are not walked
    /// for reads; every other child is. Leave `targets` empty for a
    /// construct that reads what it writes (`x += 1`, `i++`).
    Binding {
        names: Vec<&'a str>,
        targets: Vec<usize>,
    },
    /// A node whose children are each one statement (a block body). The
    /// node itself is transparent: its statements are control-dependent
    /// on the statement that contains the list, not on the list.
    StatementList,
}

/// An adapter's account of its own [`TreeNode`] label vocabulary.
///
/// Implemented once per language crate, on the labels that crate's
/// parser emits. The domain never inspects a label itself: a node is a
/// read, a binding, or a statement list because the vocabulary says so.
pub trait DependenceVocabulary: Sync {
    /// Classify one node. Only consulted for nodes *below* a statement's
    /// root that [`Self::is_statement`] did not claim.
    fn role<'a>(&self, node: &'a TreeNode) -> DependenceRole<'a>;

    /// Whether `node`, met below a statement's root and outside any
    /// list, is a statement of its own: an `else if`, the body of a
    /// braceless `if`, a Python suite member. Consecutive siblings that
    /// are form one list. Asked before [`Self::role`], so a construct
    /// that is both a statement and a binding (a Python assignment) is
    /// a statement where it stands alone and a binding at its own root.
    /// The default, for grammars that wrap every statement run in a
    /// list, is `false`.
    fn is_statement(&self, node: &TreeNode) -> bool {
        let _ = node;
        false
    }

    /// Whether the statement lists nested under `node` are loop bodies,
    /// whose end reaches their own start. Loop headers are re-read after
    /// their body so `while i < n { i += 1 }` sees the increment.
    fn is_loop(&self, node: &TreeNode) -> bool;

    /// The coarse category of a statement rooted at `node`: the label
    /// the kernel's structure half starts from. The default drops a
    /// parenthesised qualifier (`CallPath(name)` → `CallPath`,
    /// `Binary(+)` → `Binary`), so which function a statement calls is
    /// content, not wiring. Override when a vocabulary spells its
    /// qualifiers differently.
    fn kind<'a>(&self, node: &'a TreeNode) -> &'a str {
        node.label
            .split_once('(')
            .map_or(node.label.as_str(), |(head, _)| head)
    }
}

/// Tuning knobs for [`build_pdg`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PdgOptions {
    /// Fold leaf values (identifiers, literals, operators) into each
    /// statement's fingerprint, and keep local names as written instead
    /// of canonicalising them. Mirrors the APTED option of the same
    /// name: the default is rename-invariant, which is what a function
    /// comparison wants; a statement-window comparison wants the names.
    pub compare_values: bool,
}

/// Which relation an edge records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DependenceKind {
    /// `from` decides whether `to` executes.
    Control,
    /// `to` reads a name `from` bound.
    Data,
}

/// A directed dependence between two statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DependenceEdge {
    pub kind: DependenceKind,
    pub from: usize,
    pub to: usize,
}

/// One statement of the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdgNode {
    /// The statement's coarse category ([`DependenceVocabulary::kind`]
    /// of its root), with a bare read of a local canonicalised. The
    /// label the kernel's structure half starts from.
    pub kind: String,
    /// Hash of the statement's subtree with nested statements elided
    /// and, unless [`PdgOptions::compare_values`] is set, local names
    /// canonicalised. Two statements fingerprint alike when they do the
    /// same thing to (possibly differently named) locals.
    pub fingerprint: u64,
}

/// A program dependence graph. Node [`Pdg::ENTRY`] is the function
/// entry: every top-level statement is control-dependent on it, and a
/// parameter's reads are data-dependent on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdg {
    pub nodes: Vec<PdgNode>,
    pub edges: BTreeSet<DependenceEdge>,
}

impl Pdg {
    /// Index of the entry node.
    pub const ENTRY: usize = 0;

    /// Statements in the graph, not counting the entry node.
    pub fn statement_count(&self) -> usize {
        self.nodes.len().saturating_sub(1)
    }

    /// `(from, to)` pairs of the given kind, in ascending order.
    pub fn edges_of(&self, kind: DependenceKind) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.edges
            .iter()
            .filter(move |edge| edge.kind == kind)
            .map(|edge| (edge.from, edge.to))
    }

    /// The kernel features [`pdg_similarity`] compares. Computed once
    /// per graph; pairwise scoring never touches the graph again.
    pub fn features(&self) -> PdgFeatures {
        let node_count = self.nodes.len();
        let mut neighbours: Vec<Vec<(DependenceKind, Direction, usize)>> =
            vec![Vec::new(); node_count];
        for edge in &self.edges {
            if let Some(out) = neighbours.get_mut(edge.from) {
                out.push((edge.kind, Direction::Out, edge.to));
            }
            if let Some(inbound) = neighbours.get_mut(edge.to) {
                inbound.push((edge.kind, Direction::In, edge.from));
            }
        }

        // The entry node keeps its bare label throughout and is never
        // counted: as a neighbour it tells a statement it is top-level,
        // but relabelling it would fold the whole top-level statement
        // multiset into one feature that any insertion breaks.
        let mut labels: Vec<u64> = self.nodes.iter().map(|n| hash_one(&n.kind)).collect();
        let mut structure = HashMap::new();
        for iteration in 0..=WL_ITERATIONS {
            for &label in labels.iter().skip(1) {
                *structure.entry(hash_one(&(iteration, label))).or_insert(0) += 1;
            }
            if iteration == WL_ITERATIONS {
                break;
            }
            labels = (0..node_count)
                .map(|v| {
                    if v == Self::ENTRY {
                        return labels[v];
                    }
                    let mut around: Vec<(DependenceKind, Direction, u64)> = neighbours[v]
                        .iter()
                        .map(|&(kind, direction, u)| (kind, direction, labels[u]))
                        .collect();
                    around.sort_unstable();
                    hash_one(&(labels[v], around))
                })
                .collect();
        }

        let mut content = HashMap::new();
        for node in self.nodes.iter().skip(1) {
            *content.entry(node.fingerprint).or_insert(0) += 1;
        }
        PdgFeatures {
            structure,
            content,
            statement_count: self.statement_count(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Direction {
    In,
    Out,
}

/// Precomputed kernel features of one graph; see [`Pdg::features`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdgFeatures {
    /// Weisfeiler-Lehman label counts over [`WL_ITERATIONS`] rounds of
    /// the statement-kind graph, entry node excluded: the wiring.
    structure: HashMap<u64, usize>,
    /// Statement fingerprint counts, entry node excluded: the content.
    content: HashMap<u64, usize>,
    statement_count: usize,
}

impl PdgFeatures {
    pub fn statement_count(&self) -> usize {
        self.statement_count
    }
}

/// Similarity of two graphs in `[0.0, 1.0]`: the mean of the cosine
/// similarity of their Weisfeiler-Lehman feature vectors and the cosine
/// similarity of their statement-fingerprint multisets.
///
/// The two halves fail differently, which is the point of averaging
/// them. Same statements wired differently (a value threaded through a
/// different local) keeps the content half and loses structure; the
/// same wiring over different statements keeps structure and loses
/// content, and bottoms out at `0.5` — two bodies whose statement kinds
/// and dependences line up but whose statements all differ are a
/// skeleton match, not a clone. Two empty bodies score `1.0`.
///
/// # Examples
///
/// ```
/// use lens_domain::{DependenceRole, DependenceVocabulary, PdgOptions, TreeNode, build_pdg, pdg_similarity};
///
/// // A toy vocabulary: `Let(name)` binds its value, `Ref(name)` reads it.
/// struct Toy;
/// impl DependenceVocabulary for Toy {
///     fn role<'a>(&self, node: &'a TreeNode) -> DependenceRole<'a> {
///         match node.label.as_str() {
///             "Let" => DependenceRole::Binding { names: vec![&node.value], targets: vec![] },
///             "Ref" => DependenceRole::Reference(&node.value),
///             _ => DependenceRole::Plain,
///         }
///     }
///     fn is_loop(&self, _: &TreeNode) -> bool { false }
/// }
///
/// let stmt = |name: &str, reads: &str| {
///     TreeNode::with_children("Let", name, vec![TreeNode::new("Ref", reads)])
/// };
/// // `let a = x; let b = y; let c = a` versus the same with the first two swapped.
/// let forward = TreeNode::with_children("Block", "", vec![stmt("a", "x"), stmt("b", "y"), stmt("c", "a")]);
/// let swapped = TreeNode::with_children("Block", "", vec![stmt("b", "y"), stmt("a", "x"), stmt("c", "a")]);
///
/// let opts = PdgOptions::default();
/// let a = build_pdg(&forward, &["x", "y"], &Toy, opts).features();
/// let b = build_pdg(&swapped, &["x", "y"], &Toy, opts).features();
/// assert!((pdg_similarity(&a, &b) - 1.0).abs() < 1e-9);
/// ```
pub fn pdg_similarity(a: &PdgFeatures, b: &PdgFeatures) -> f64 {
    let structure = cosine(&a.structure, &b.structure);
    let content = cosine(&a.content, &b.content);
    ((structure + content) / 2.0).clamp(0.0, 1.0)
}

/// Cosine similarity of two count vectors. Two empty vectors are
/// identical rather than undefined; one empty side scores zero.
fn cosine(a: &HashMap<u64, usize>, b: &HashMap<u64, usize>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let dot: f64 = a
        .iter()
        .filter_map(|(key, &count_a)| b.get(key).map(|&count_b| (count_a * count_b) as f64))
        .sum();
    let norm = |v: &HashMap<u64, usize>| v.values().map(|&c| (c * c) as f64).sum::<f64>().sqrt();
    let denominator = norm(a) * norm(b);
    if denominator == 0.0 {
        0.0
    } else {
        dot / denominator
    }
}

/// Build the dependence graph of a function body.
///
/// `body` is the block the adapter lowered the function to, whose
/// children are its top-level statements. `parameters` are bound at the
/// entry node, so a read of one is a data edge from [`Pdg::ENTRY`] and
/// a read of a free name (a function, a global) is no edge at all.
///
/// Data edges are reaching definitions computed in statement order:
/// a statement reads under the definitions current before it, then
/// installs its own. A nested list starts from the definitions current
/// at its containing statement, and what it leaves behind is merged
/// back (unioned, never replaced — a branch may not run), so a read
/// after an `if` depends on every arm and on what came before. A loop
/// body is flowed twice, so a definition at its end reaches a read at
/// its start, and a loop statement's own reads are repeated after its
/// body.
pub fn build_pdg(
    body: &TreeNode,
    parameters: &[&str],
    vocabulary: &dyn DependenceVocabulary,
    opts: PdgOptions,
) -> Pdg {
    let bound = if opts.compare_values {
        HashSet::new()
    } else {
        bound_names(body, parameters, vocabulary)
    };
    let mut builder = Builder {
        vocabulary,
        opts,
        bound: &bound,
        nodes: vec![PdgNode {
            kind: ENTRY_KIND.to_owned(),
            fingerprint: hash_one(&ENTRY_KIND),
        }],
        edges: BTreeSet::new(),
        skeleton: vec![StatementInfo::default()],
    };
    let top_level = builder.collect_list(body, Pdg::ENTRY);

    let mut defs: Definitions = parameters
        .iter()
        .map(|name| ((*name).to_owned(), BTreeSet::from([Pdg::ENTRY])))
        .collect();
    flow(&builder.skeleton, &mut builder.edges, &top_level, &mut defs);

    Pdg {
        nodes: builder.nodes,
        edges: builder.edges,
    }
}

const ENTRY_KIND: &str = "Entry";
/// Placeholder a canonicalised local read hashes as.
const REFERENCE_MARK: &str = "\u{0}ref";
/// Placeholder an elided nested statement hashes as.
const NESTED_MARK: &str = "\u{0}nested";

/// Name → statements whose binding of it may be the one a read sees.
type Definitions = HashMap<String, BTreeSet<usize>>;

/// Every name bound anywhere in the body, plus the parameters: the set
/// of names a fingerprint canonicalises. A free name stays as written,
/// so calling a different function is different content while reading
/// a differently named local is not.
fn bound_names(
    body: &TreeNode,
    parameters: &[&str],
    vocabulary: &dyn DependenceVocabulary,
) -> HashSet<String> {
    fn walk(node: &TreeNode, vocabulary: &dyn DependenceVocabulary, out: &mut HashSet<String>) {
        if let DependenceRole::Binding { names, .. } = vocabulary.role(node) {
            out.extend(names.into_iter().map(str::to_owned));
        }
        for child in &node.children {
            walk(child, vocabulary, out);
        }
    }
    let mut out: HashSet<String> = parameters.iter().map(|name| (*name).to_owned()).collect();
    walk(body, vocabulary, &mut out);
    out
}

/// What one statement reads, binds, and contains, gathered by the scan
/// so the dataflow pass never re-walks the tree.
#[derive(Debug, Default)]
struct StatementInfo {
    uses: Vec<String>,
    defs: Vec<String>,
    nested: Vec<NestedList>,
}

#[derive(Debug)]
struct NestedList {
    statements: Vec<usize>,
    is_loop: bool,
}

struct Builder<'v> {
    vocabulary: &'v dyn DependenceVocabulary,
    opts: PdgOptions,
    bound: &'v HashSet<String>,
    nodes: Vec<PdgNode>,
    edges: BTreeSet<DependenceEdge>,
    /// Indexed by node id; the entry's slot is empty.
    skeleton: Vec<StatementInfo>,
}

/// Accumulator for one statement's scan.
struct Scan {
    info: StatementInfo,
    hasher: std::collections::hash_map::DefaultHasher,
}

impl Builder<'_> {
    /// Add every child of `list` as a statement under `parent`, splicing
    /// nested lists flat, and return the statement ids in order.
    fn collect_list(&mut self, list: &TreeNode, parent: usize) -> Vec<usize> {
        let mut ids = Vec::with_capacity(list.children.len());
        for child in &list.children {
            if self.vocabulary.role(child) == DependenceRole::StatementList {
                ids.extend(self.collect_list(child, parent));
            } else {
                ids.push(self.add_statement(child, parent));
            }
        }
        ids
    }

    /// Mint the node for `root`, record its control dependence on
    /// `parent`, and scan it for reads, bindings, and nested lists.
    fn add_statement(&mut self, root: &TreeNode, parent: usize) -> usize {
        let id = self.nodes.len();
        self.nodes.push(PdgNode {
            kind: String::new(),
            fingerprint: 0,
        });
        self.skeleton.push(StatementInfo::default());
        self.edges.insert(DependenceEdge {
            kind: DependenceKind::Control,
            from: parent,
            to: id,
        });

        let mut scan = Scan {
            info: StatementInfo::default(),
            hasher: std::collections::hash_map::DefaultHasher::new(),
        };
        // The root is the statement; a list only reaches here through a
        // vocabulary that spells a bare block as one, and is then plain
        // structure.
        let root_role = match self.vocabulary.role(root) {
            DependenceRole::StatementList => DependenceRole::Plain,
            role => role,
        };
        let kind = match root_role {
            DependenceRole::Reference(name) if self.bound.contains(name) => {
                REFERENCE_MARK.to_owned()
            }
            _ => self.vocabulary.kind(root).to_owned(),
        };
        self.visit(root, root_role, id, false, &mut scan);

        let fingerprint = scan.hasher.finish();
        self.nodes[id] = PdgNode { kind, fingerprint };
        self.skeleton[id] = scan.info;
        id
    }

    /// Walk one node of statement `owner`. `in_loop` is whether a loop
    /// construct sits between the statement root and this node, which
    /// makes any list found below a loop body.
    fn visit(
        &mut self,
        node: &TreeNode,
        role: DependenceRole<'_>,
        owner: usize,
        in_loop: bool,
        scan: &mut Scan,
    ) {
        match role {
            DependenceRole::StatementList => {
                NESTED_MARK.hash(&mut scan.hasher);
                let statements = self.collect_list(node, owner);
                scan.info.nested.push(NestedList {
                    statements,
                    is_loop: in_loop,
                });
            }
            DependenceRole::Reference(name) => {
                scan.info.uses.push(name.to_owned());
                if self.bound.contains(name) {
                    REFERENCE_MARK.hash(&mut scan.hasher);
                } else {
                    self.hash_node(node, &mut scan.hasher);
                }
                self.visit_children(node, &[], owner, in_loop, scan);
            }
            DependenceRole::Binding { names, targets } => {
                scan.info.defs.extend(names.into_iter().map(str::to_owned));
                self.hash_node(node, &mut scan.hasher);
                self.visit_children(node, &targets, owner, in_loop, scan);
            }
            DependenceRole::Plain => {
                self.hash_node(node, &mut scan.hasher);
                self.visit_children(node, &[], owner, in_loop, scan);
            }
        }
    }

    fn visit_children(
        &mut self,
        node: &TreeNode,
        targets: &[usize],
        owner: usize,
        in_loop: bool,
        scan: &mut Scan,
    ) {
        let in_loop = in_loop || self.vocabulary.is_loop(node);
        // Consecutive loose statements are one list, so a Python suite
        // flows in order and loops back as one.
        let mut run_open = false;
        for (index, child) in node.children.iter().enumerate() {
            if targets.contains(&index) {
                self.hash_subtree(child, &mut scan.hasher);
                run_open = false;
                continue;
            }
            if !self.vocabulary.is_statement(child) {
                let role = self.vocabulary.role(child);
                self.visit(child, role, owner, in_loop, scan);
                run_open = false;
                continue;
            }
            NESTED_MARK.hash(&mut scan.hasher);
            let id = self.add_statement(child, owner);
            match scan.info.nested.last_mut() {
                Some(list) if run_open => list.statements.push(id),
                _ => scan.info.nested.push(NestedList {
                    statements: vec![id],
                    is_loop: in_loop,
                }),
            }
            run_open = true;
        }
    }

    fn hash_node(&self, node: &TreeNode, hasher: &mut impl Hasher) {
        node.label.hash(hasher);
        if self.opts.compare_values {
            node.value.hash(hasher);
        }
        node.children.len().hash(hasher);
    }

    /// Hash a binding target as structure only: its identifiers are
    /// the names being bound, which a rename-invariant fingerprint must
    /// not see.
    fn hash_subtree(&self, node: &TreeNode, hasher: &mut impl Hasher) {
        self.hash_node(node, hasher);
        for child in &node.children {
            self.hash_subtree(child, hasher);
        }
    }
}

/// Reaching-definitions pass over one statement list; see
/// [`build_pdg`] for the merge rules.
fn flow(
    skeleton: &[StatementInfo],
    edges: &mut BTreeSet<DependenceEdge>,
    list: &[usize],
    defs: &mut Definitions,
) {
    for &id in list {
        let Some(info) = skeleton.get(id) else {
            continue;
        };
        add_use_edges(edges, info, id, defs);
        for name in &info.defs {
            defs.insert(name.clone(), BTreeSet::from([id]));
        }

        if info.nested.is_empty() {
            continue;
        }
        let mut merged = defs.clone();
        let mut has_loop_body = false;
        for nested in &info.nested {
            let mut inner = defs.clone();
            flow(skeleton, edges, &nested.statements, &mut inner);
            if nested.is_loop {
                has_loop_body = true;
                flow(skeleton, edges, &nested.statements, &mut inner);
            }
            for (name, sites) in inner {
                merged.entry(name).or_default().extend(sites);
            }
        }
        if has_loop_body {
            add_use_edges(edges, info, id, &merged);
        }
        *defs = merged;
    }
}

fn add_use_edges(
    edges: &mut BTreeSet<DependenceEdge>,
    info: &StatementInfo,
    id: usize,
    defs: &Definitions,
) {
    for name in &info.uses {
        let Some(sites) = defs.get(name) else {
            continue;
        };
        for &from in sites {
            edges.insert(DependenceEdge {
                kind: DependenceKind::Data,
                from,
                to: id,
            });
        }
    }
}

fn hash_one(value: &impl Hash) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::collection::vec as prop_vec;
    use proptest::prelude::*;
    use rstest::rstest;

    /// A toy vocabulary with one construct per role:
    /// `Block` is a list, `Ref(name)` a read, `Let(name)` / `Set(name)`
    /// bind their value (with `Set` also reading it, like `x += 1`),
    /// `Stmt` wraps a loose statement, `If` and `Loop` are control.
    struct Toy;

    impl DependenceVocabulary for Toy {
        fn role<'a>(&self, node: &'a TreeNode) -> DependenceRole<'a> {
            match node.label.as_str() {
                "Block" => DependenceRole::StatementList,
                "Ref" => DependenceRole::Reference(&node.value),
                // A read with the name in the label, the way the Rust
                // adapter spells `Path(x)`.
                label if label.starts_with("Ref(") && label.ends_with(')') => {
                    DependenceRole::Reference(&label[4..label.len() - 1])
                }
                "Let" => DependenceRole::Binding {
                    names: vec![&node.value],
                    targets: vec![0],
                },
                "Set" => DependenceRole::Binding {
                    names: vec![&node.value],
                    targets: vec![],
                },
                _ => DependenceRole::Plain,
            }
        }

        fn is_loop(&self, node: &TreeNode) -> bool {
            node.label == "Loop"
        }

        fn is_statement(&self, node: &TreeNode) -> bool {
            node.label == "Stmt"
        }
    }

    fn block(stmts: Vec<TreeNode>) -> TreeNode {
        TreeNode::with_children("Block", "", stmts)
    }

    fn r(name: &str) -> TreeNode {
        TreeNode::new("Ref", name)
    }

    /// A read carrying its name in the label rather than the value.
    fn rl(name: &str) -> TreeNode {
        TreeNode::leaf(format!("Ref({name})"))
    }

    fn pat(name: &str) -> TreeNode {
        TreeNode::new("Pat", name)
    }

    /// `let name = <init>`: the pattern is child 0 (skipped for reads).
    fn let_(name: &str, init: TreeNode) -> TreeNode {
        TreeNode::with_children("Let", name, vec![pat(name), init])
    }

    /// `name = <value>` where the value may read `name` itself.
    fn set(name: &str, value: TreeNode) -> TreeNode {
        TreeNode::with_children("Set", name, vec![value])
    }

    /// A call with the callee in the label, the way the Rust adapter
    /// spells `CallPath(f)`: a free name is structure, not a value.
    fn call(callee: &str, args: Vec<TreeNode>) -> TreeNode {
        TreeNode::with_children(format!("Call({callee})"), "", args)
    }

    fn if_(cond: TreeNode, branches: Vec<TreeNode>) -> TreeNode {
        let mut children = vec![cond];
        children.extend(branches);
        TreeNode::with_children("If", "", children)
    }

    fn loop_(cond: TreeNode, body: TreeNode) -> TreeNode {
        TreeNode::with_children("Loop", "", vec![cond, body])
    }

    fn stmt(inner: TreeNode) -> TreeNode {
        TreeNode::with_children("Stmt", "", vec![inner])
    }

    fn graph(body: &TreeNode, params: &[&str]) -> Pdg {
        build_pdg(body, params, &Toy, PdgOptions::default())
    }

    fn data(pdg: &Pdg) -> Vec<(usize, usize)> {
        pdg.edges_of(DependenceKind::Data).collect()
    }

    fn control(pdg: &Pdg) -> Vec<(usize, usize)> {
        pdg.edges_of(DependenceKind::Control).collect()
    }

    fn similarity(a: &TreeNode, b: &TreeNode, params: &[&str], opts: PdgOptions) -> f64 {
        let fa = build_pdg(a, params, &Toy, opts).features();
        let fb = build_pdg(b, params, &Toy, opts).features();
        pdg_similarity(&fa, &fb)
    }

    /// `let a = f(x); let b = g(a); h(b)` with `x` a parameter.
    fn chain() -> TreeNode {
        block(vec![
            let_("a", call("f", vec![r("x")])),
            let_("b", call("g", vec![r("a")])),
            call("h", vec![r("b")]),
        ])
    }

    #[test]
    fn straight_line_body_chains_data_edges_from_the_entry() {
        let pdg = graph(&chain(), &["x"]);
        assert_eq!(pdg.statement_count(), 3);
        assert_eq!(control(&pdg), vec![(0, 1), (0, 2), (0, 3)]);
        assert_eq!(data(&pdg), vec![(0, 1), (1, 2), (2, 3)]);
    }

    #[test]
    fn a_free_name_has_no_definition_and_no_edge() {
        // `x` is not a parameter here, so its read depends on nothing.
        let pdg = graph(&chain(), &[]);
        assert_eq!(data(&pdg), vec![(1, 2), (2, 3)]);
    }

    #[test]
    fn a_later_binding_shadows_the_earlier_one() {
        let body = block(vec![
            let_("a", call("f", vec![])),
            let_("a", call("g", vec![])),
            call("h", vec![r("a")]),
        ]);
        let pdg = graph(&body, &[]);
        assert_eq!(data(&pdg), vec![(2, 3)]);
    }

    #[test]
    fn nested_statements_are_control_dependent_on_their_predicate() {
        // `if c { let y = f(x) }`
        let body = block(vec![if_(
            r("c"),
            vec![block(vec![let_("y", call("f", vec![r("x")]))])],
        )]);
        let pdg = graph(&body, &["c", "x"]);
        assert_eq!(control(&pdg), vec![(0, 1), (1, 2)]);
        assert_eq!(data(&pdg), vec![(0, 1), (0, 2)]);
    }

    #[test]
    fn a_read_after_a_branch_sees_every_arm_and_what_came_before() {
        // `let x = f(); if c { x = g() } else { x = h() }; k(x)`
        let body = block(vec![
            let_("x", call("f", vec![])),
            if_(
                r("c"),
                vec![
                    block(vec![set("x", call("g", vec![]))]),
                    block(vec![set("x", call("h", vec![]))]),
                ],
            ),
            call("k", vec![r("x")]),
        ]);
        let pdg = graph(&body, &["c"]);
        // Nodes: 1 = let, 2 = if, 3 = then-arm set, 4 = else-arm set, 5 = k(x).
        assert_eq!(control(&pdg), vec![(0, 1), (0, 2), (0, 5), (2, 3), (2, 4)]);
        assert_eq!(data(&pdg), vec![(0, 2), (1, 5), (3, 5), (4, 5)]);
    }

    #[test]
    fn a_loop_body_reaches_its_own_start() {
        // `loop (i < n) { let t = f(i); i = g(t) }`
        let body = block(vec![loop_(
            call("lt", vec![r("i"), r("n")]),
            block(vec![
                let_("t", call("f", vec![r("i")])),
                set("i", call("g", vec![r("t")])),
            ]),
        )]);
        let pdg = graph(&body, &["i", "n"]);
        // 1 = loop, 2 = let t, 3 = i = g(t).
        assert_eq!(control(&pdg), vec![(0, 1), (1, 2), (1, 3)]);
        // The header reads `i` from the entry and from the body's
        // assignment; `let t` likewise; the assignment reads `t`.
        assert_eq!(data(&pdg), vec![(0, 1), (0, 2), (2, 3), (3, 1), (3, 2)]);
    }

    #[test]
    fn a_binding_that_reads_itself_depends_on_its_own_previous_run_in_a_loop() {
        let body = block(vec![loop_(r("c"), block(vec![set("i", r("i"))]))]);
        let pdg = graph(&body, &["c", "i"]);
        assert!(data(&pdg).contains(&(2, 2)), "got {:?}", data(&pdg));
    }

    #[test]
    fn loose_consecutive_statements_flow_as_one_list() {
        // A Python-shaped loop: the body statements are siblings of the
        // condition, not children of a list. The second statement binds
        // `y`, which the first reads, so grouping them is what lets the
        // back-edge exist.
        let body = block(vec![loop_(
            r("c"),
            TreeNode::with_children(
                "Suite",
                "",
                vec![
                    stmt(set("x", call("f", vec![r("y")]))),
                    stmt(set("y", call("g", vec![r("x")]))),
                ],
            ),
        )]);
        let pdg = graph(&body, &["c"]);
        assert_eq!(control(&pdg), vec![(0, 1), (1, 2), (1, 3)]);
        assert_eq!(data(&pdg), vec![(0, 1), (2, 3), (3, 2)]);
    }

    #[test]
    fn a_binding_target_is_not_a_read() {
        // `let x = f(x)` where the pattern child would otherwise read the
        // name it binds. Only the initialiser reads the parameter.
        let body = block(vec![let_("x", call("f", vec![r("x")]))]);
        let pdg = graph(&body, &["x"]);
        assert_eq!(data(&pdg), vec![(0, 1)]);
        assert_eq!(pdg.nodes[1].kind, "Let");
    }

    #[test]
    fn a_bare_read_statement_takes_the_canonical_kind() {
        let body = block(vec![r("x")]);
        let pdg = graph(&body, &["x"]);
        assert_eq!(pdg.nodes[1].kind, REFERENCE_MARK);
        let free = graph(&body, &[]);
        assert_eq!(free.nodes[1].kind, "Ref");
    }

    #[test]
    fn a_statement_kind_drops_the_label_qualifier() {
        let pdg = graph(&chain(), &["x"]);
        assert_eq!(pdg.nodes[3].kind, "Call");
        // ...but the callee still tells the fingerprints apart.
        let other = graph(&block(vec![call("k", vec![r("b")])]), &["x"]);
        assert_ne!(pdg.nodes[3].fingerprint, other.nodes[1].fingerprint);
    }

    #[test]
    fn a_label_carried_local_name_is_canonicalised() {
        // The names differ only in the label of the read, which a
        // rename-invariant fingerprint must see through — but only for
        // a name the body binds.
        let named = |name: &str| {
            block(vec![
                let_(name, call("f", vec![rl("x")])),
                call("g", vec![rl(name)]),
            ])
        };
        let invariant = similarity(&named("a"), &named("b"), &["x"], PdgOptions::default());
        assert!((invariant - 1.0).abs() < 1e-9, "got {invariant}");
        let literal = similarity(
            &named("a"),
            &named("b"),
            &["x"],
            PdgOptions {
                compare_values: true,
            },
        );
        assert!(literal < 1.0, "got {literal}");
    }

    #[test]
    fn binding_targets_shape_the_fingerprint() {
        // Same binding construct, different pattern shape: a tuple
        // pattern is structure the fingerprint keeps, even though the
        // names inside it are not.
        let plain = block(vec![let_("a", call("f", vec![]))]);
        let tuple = block(vec![TreeNode::with_children(
            "Let",
            "a",
            vec![
                TreeNode::with_children("PatTuple", "", vec![pat("a"), pat("b")]),
                call("f", vec![]),
            ],
        )]);
        let plain = graph(&plain, &[]);
        let tuple = graph(&tuple, &[]);
        assert_ne!(plain.nodes[1].fingerprint, tuple.nodes[1].fingerprint);
    }

    #[test]
    fn a_plain_sibling_splits_a_run_of_loose_statements() {
        // Two loose statements with an expression between them are two
        // lists, so the second does not read what the first bound.
        let body = block(vec![if_(
            r("c"),
            vec![TreeNode::with_children(
                "Suite",
                "",
                vec![
                    stmt(set("x", call("f", vec![]))),
                    r("c"),
                    stmt(call("g", vec![r("x")])),
                ],
            )],
        )]);
        let pdg = graph(&body, &["c"]);
        assert_eq!(control(&pdg), vec![(0, 1), (1, 2), (1, 3)]);
        assert_eq!(data(&pdg), vec![(0, 1)]);
    }

    /// A vocabulary that leaves `is_statement` at its default.
    struct ListsOnly;

    impl DependenceVocabulary for ListsOnly {
        fn role<'a>(&self, node: &'a TreeNode) -> DependenceRole<'a> {
            Toy.role(node)
        }

        fn is_loop(&self, node: &TreeNode) -> bool {
            Toy.is_loop(node)
        }
    }

    #[test]
    fn is_statement_defaults_to_false() {
        // Without the override the `Stmt` wrapper is plain structure,
        // so the binding inside it folds into the `if` statement.
        let body = block(vec![if_(r("c"), vec![stmt(let_("y", r("c")))])]);
        let pdg = build_pdg(&body, &["c"], &ListsOnly, PdgOptions::default());
        assert_eq!(pdg.statement_count(), 1);
        assert_eq!(data(&pdg), vec![(0, 1)]);
    }

    #[test]
    fn empty_body_is_just_the_entry() {
        let pdg = graph(&block(vec![]), &["x"]);
        assert_eq!(pdg.statement_count(), 0);
        assert!(pdg.edges.is_empty());
    }

    // --- similarity ---

    #[test]
    fn identical_bodies_score_one() {
        let s = similarity(&chain(), &chain(), &["x"], PdgOptions::default());
        assert!((s - 1.0).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn reordered_independent_statements_score_one() {
        let forward = block(vec![
            let_("a", call("f", vec![r("x")])),
            let_("b", call("g", vec![r("y")])),
            call("h", vec![r("a"), r("b")]),
        ]);
        let swapped = block(vec![
            let_("b", call("g", vec![r("y")])),
            let_("a", call("f", vec![r("x")])),
            call("h", vec![r("a"), r("b")]),
        ]);
        let s = similarity(&forward, &swapped, &["x", "y"], PdgOptions::default());
        assert!((s - 1.0).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn renamed_locals_score_one_unless_values_are_compared() {
        let renamed = block(vec![
            let_("first", call("f", vec![r("x")])),
            let_("second", call("g", vec![r("first")])),
            call("h", vec![r("second")]),
        ]);
        let invariant = similarity(&chain(), &renamed, &["x"], PdgOptions::default());
        assert!((invariant - 1.0).abs() < 1e-9, "got {invariant}");

        let literal = similarity(
            &chain(),
            &renamed,
            &["x"],
            PdgOptions {
                compare_values: true,
            },
        );
        assert!(literal < 1.0, "got {literal}");
        // The wiring is untouched, so only the content half moves.
        assert!(literal >= 0.5, "got {literal}");
    }

    #[test]
    fn a_free_name_is_content() {
        // Same wiring, but the middle statement calls a different
        // function: the structure half holds, the content half slips.
        let other = block(vec![
            let_("a", call("f", vec![r("x")])),
            let_("b", call("other", vec![r("a")])),
            call("h", vec![r("b")]),
        ]);
        let s = similarity(&chain(), &other, &["x"], PdgOptions::default());
        assert!(s < 1.0 && s > 0.5, "got {s}");
    }

    #[test]
    fn rewired_statements_lose_structure_but_keep_content() {
        // Same three statements; `b` now reads `x` instead of `a`.
        let rewired = block(vec![
            let_("a", call("f", vec![r("x")])),
            let_("b", call("g", vec![r("x")])),
            call("h", vec![r("b")]),
        ]);
        let s = similarity(&chain(), &rewired, &["x"], PdgOptions::default());
        assert!(s < 1.0 && s > 0.5, "got {s}");
    }

    #[test]
    fn same_skeleton_over_different_statements_scores_half() {
        let other = block(vec![
            let_("a", call("p", vec![r("x")])),
            let_("b", call("q", vec![r("a")])),
            call("s", vec![r("b")]),
        ]);
        let s = similarity(&chain(), &other, &["x"], PdgOptions::default());
        assert!((s - 0.5).abs() < 1e-9, "got {s}");
    }

    #[rstest]
    #[case::both_empty(block(vec![]), block(vec![]), 1.0)]
    #[case::one_empty(block(vec![]), chain(), 0.0)]
    fn empty_bodies(#[case] a: TreeNode, #[case] b: TreeNode, #[case] expected: f64) {
        let s = similarity(&a, &b, &["x"], PdgOptions::default());
        assert!((s - expected).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn an_inserted_statement_costs_less_than_a_rewrite() {
        let mut extended = chain();
        extended.children.insert(1, call("log", vec![r("a")]));
        let inserted = similarity(&chain(), &extended, &["x"], PdgOptions::default());
        let rewritten = similarity(
            &chain(),
            &block(vec![
                let_("a", call("p", vec![r("x")])),
                let_("b", call("q", vec![r("x")])),
                call("s", vec![r("a"), r("b")]),
            ]),
            &["x"],
            PdgOptions::default(),
        );
        assert!(
            inserted > rewritten,
            "inserted {inserted} vs rewritten {rewritten}"
        );
        assert!(inserted > 0.7, "got {inserted}");
    }

    #[test]
    fn features_report_the_statement_count() {
        let features = graph(&chain(), &["x"]).features();
        assert_eq!(features.statement_count(), 3);
    }

    fn arb_body() -> impl Strategy<Value = TreeNode> {
        let name = prop_oneof![Just("a"), Just("b"), Just("c"), Just("x")];
        let leaf = name.clone().prop_map(r);
        let expr = leaf.prop_recursive(3, 12, 3, |inner| {
            (prop_oneof![Just("f"), Just("g")], prop_vec(inner, 0..3))
                .prop_map(|(callee, args)| call(callee, args))
        });
        let stmt = expr.prop_recursive(3, 24, 3, move |inner| {
            prop_oneof![
                (name.clone(), inner.clone()).prop_map(|(n, e)| let_(n, e)),
                (name.clone(), inner.clone()).prop_map(|(n, e)| set(n, e)),
                (inner.clone(), prop_vec(inner.clone(), 1..3)).prop_map(|(c, arms)| if_(
                    c,
                    arms.into_iter().map(|s| block(vec![s])).collect()
                )),
                (inner.clone(), inner.clone()).prop_map(|(c, s)| loop_(c, block(vec![s]))),
                inner,
            ]
        });
        prop_vec(stmt, 0..6).prop_map(block)
    }

    proptest! {
        #[test]
        fn similarity_is_reflexive_symmetric_and_bounded(a in arb_body(), b in arb_body()) {
            let opts = PdgOptions::default();
            let fa = build_pdg(&a, &["x"], &Toy, opts).features();
            let fb = build_pdg(&b, &["x"], &Toy, opts).features();
            let ab = pdg_similarity(&fa, &fb);
            let ba = pdg_similarity(&fb, &fa);
            prop_assert!((0.0..=1.0).contains(&ab), "out of range: {ab}");
            prop_assert!((ab - ba).abs() < 1e-9, "asymmetric: {ab} vs {ba}");
            prop_assert!((pdg_similarity(&fa, &fa) - 1.0).abs() < 1e-9, "not reflexive");
        }

        #[test]
        fn every_edge_points_at_a_node_and_every_statement_is_controlled(body in arb_body()) {
            let pdg = build_pdg(&body, &["x"], &Toy, PdgOptions::default());
            let n = pdg.nodes.len();
            for edge in &pdg.edges {
                prop_assert!(edge.from < n && edge.to < n, "dangling {edge:?}");
            }
            for id in 1..n {
                prop_assert!(
                    pdg.edges.iter().any(|e| e.kind == DependenceKind::Control && e.to == id),
                    "statement {id} has no controlling predicate"
                );
            }
            prop_assert!(!pdg.edges.iter().any(|e| e.kind == DependenceKind::Control && e.to == Pdg::ENTRY));
        }

        #[test]
        fn shuffling_top_level_statements_never_changes_the_content_half(body in arb_body()) {
            // Content is a multiset of statement fingerprints, so any
            // permutation of the top level keeps it; structure may move.
            let mut reversed = body.clone();
            reversed.children.reverse();
            let opts = PdgOptions::default();
            let fa = build_pdg(&body, &["x"], &Toy, opts).features();
            let fb = build_pdg(&reversed, &["x"], &Toy, opts).features();
            prop_assert_eq!(fa.content, fb.content);
        }
    }
}
