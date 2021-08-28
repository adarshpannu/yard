#![allow(warnings)]

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::thread::JoinHandle;
use typed_arena::Arena;

use crate::csv::*;
use crate::expr::{Expr::*, *};
use crate::graphviz::{htmlify, write_flow_to_graphviz};
use crate::includes::*;
use crate::row::*;
use crate::task::*;

type NodeArena = Arena<Node>;

#[derive(Debug, Serialize)]
enum NodeEnum {
    EmitNode(EmitNode),
    CSVNode(CSVNode),
    ProjectNode(ProjectNode),
    FilterNode(FilterNode),
    AggNode(AggNode),
    JoinNode(JoinNode),
}

#[derive(Debug, Serialize)]
pub struct Node {
    id: NodeId,
    children: Vec<NodeId>,
    npartitions: usize,
    node_enum: NodeEnum,
}

impl Node {
    fn base(&self) -> &Self {
        self
    }

    fn new0(
        arena: &NodeArena, npartitions: usize, node_enum: NodeEnum,
    ) -> &Node {
        let id = arena.len();
        arena.alloc(Node {
            id,
            children: vec![],
            npartitions,
            node_enum,
        })
    }

    fn new1(
        arena: &NodeArena, child_id: NodeId, npartitions: usize,
        node_enum: NodeEnum,
    ) -> &Node {
        let id = arena.len();
        arena.alloc(Node {
            id,
            children: vec![child_id],
            npartitions,
            node_enum,
        })
    }

    fn new<'a>(
        arena: &'a NodeArena, child_id: NodeId, other_children: Vec<&Node>,
        npartitions: usize, node_enum: NodeEnum,
    ) -> &'a Node {
        let id = arena.len();
        let mut children: Vec<_> =
            other_children.iter().map(|e| e.id()).collect();
        children.push(child_id);
        arena.alloc(Node {
            id,
            children,
            npartitions,
            node_enum,
        })
    }
}

/***************************************************************************************************/
impl Node {
    fn emit<'a>(&self, arena: &'a NodeArena) -> &'a Node {
        let npartitions = self.npartitions;
        let retval =
            Node::new1(&arena, self.id(), npartitions, EmitNode::new());
        retval
    }

    fn project<'a>(&self, arena: &'a NodeArena, colids: Vec<ColId>) -> &'a Node {
        let npartitions = self.base().npartitions;
        let retval = Node::new1(
            &arena,
            self.id(),
            npartitions,
            ProjectNode::new(colids),
        );
        retval
    }

    fn filter<'a>(&self, arena: &'a NodeArena, expr: Expr) -> &'a Node {
        let npartitions = self.base().npartitions;
        let retval =
            Node::new1(&arena, self.id(), npartitions, FilterNode::new(expr));
        retval
    }

    fn join<'a>(
        &self, arena: &'a NodeArena, other_children: Vec<&Node>,
        preds: Vec<JoinPredicate>,
    ) -> &'a Node {
        let retval = Node::new(
            &arena,
            self.id(),
            other_children,
            self.npartitions, // TBD: Partitions need to be decided
            JoinNode::new(preds),
        );
        retval
    }

    fn agg<'a>(
        &self, arena: &'a NodeArena, keycolids: Vec<ColId>,
        aggcolids: Vec<(AggType, ColId)>, npartitions: usize,
    ) -> &'a Node {
        let aggnode = AggNode::new(keycolids, aggcolids);
        let retval = Node::new1(&arena, self.id(), npartitions, aggnode);
        retval
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn children(&self) -> &Vec<NodeId> {
        &self.children
    }

    pub fn desc(&self) -> String {
        unimplemented!()
    }

    pub fn nchildren(&self) -> usize {
        self.base().children.len()
    }

    pub fn child<'a>(&self, flow: &'a Flow, ix: NodeId) -> &'a Node {
        let children = &self.base().children;
        flow.get_node(children[ix])
    }

    pub fn next(
        &self, flow: &Flow, stage: &Stage, task: &mut Task, is_head: bool,
    ) -> Option<Row> {
        // stupid dispatch, ugh!
        match &self.node_enum {
            NodeEnum::CSVNode(inner_node) => inner_node.next(self, flow, stage, task, is_head),
            NodeEnum::EmitNode(inner_node) => inner_node.next(self, flow, stage, task, is_head),
            NodeEnum::ProjectNode(inner_node) => inner_node.next(self, flow, stage, task, is_head),
            NodeEnum::FilterNode(inner_node) => inner_node.next(self, flow, stage, task, is_head),
            NodeEnum::JoinNode(inner_node) => inner_node.next(self, flow, stage, task, is_head),
            NodeEnum::AggNode(inner_node) => inner_node.next(self, flow, stage, task, is_head),
        }
    }

    pub fn is_endpoint(&self) -> bool {
        match &self.node_enum {
            NodeEnum::EmitNode(_) => true,
            NodeEnum::AggNode(_) => true,
            _ => false
        }
    }

    pub fn npartitions(&self) -> usize {
        self.base().npartitions
    }
}

/***************************************************************************************************/
pub enum NodeRuntime {
    Unused,
    CSV { iter: CSVPartitionIter },
}

/***************************************************************************************************/
#[derive(Debug, Serialize)]
struct EmitNode {}

impl EmitNode {
    fn new() -> NodeEnum {
        NodeEnum::EmitNode(EmitNode {})
    }

    fn desc(&self, supernode: &Node) -> String {
        format!("EmitNode-#{}", supernode.id())
    }

    fn next(
        &self, supernode: &Node, flow: &Flow, stage: &Stage, task: &mut Task,
        is_head: bool,
    ) -> Option<Row> {
        supernode.child(flow, 0).next(flow, stage, task, false)
    }
}

impl EmitNode {}

/***************************************************************************************************/
#[derive(Debug, Serialize)]
struct CSVNode {
    filename: String,
    colnames: Vec<String>,
    coltypes: Vec<DataType>,
    partitions: Vec<TextFilePartition>,
}

use std::fs::File;
use std::io::{self, BufRead};
use std::path::Path;

fn read_lines<P>(filename: P) -> io::Result<io::Lines<io::BufReader<File>>>
where
    P: AsRef<Path>,
{
    let file = File::open(filename)?;
    Ok(io::BufReader::new(file).lines())
}

impl CSVNode {
    fn new<'a>(arena: &'a NodeArena, filename: String, npartitions: usize) -> &'a Node {
        let (colnames, coltypes) = Self::infer_metadata(&filename);

        let partitions =
            compute_partitions(&filename, npartitions as u64).unwrap();
        let csvnode = NodeEnum::CSVNode(CSVNode {
            filename,
            colnames,
            coltypes,
            partitions,
        });
        let node = Node::new0(arena, npartitions, csvnode);
        node
    }

    fn infer_datatype(str: &String) -> DataType {
        let res = str.parse::<i32>();
        if res.is_ok() {
            DataType::INT
        } else if str.eq("true") || str.eq("false") {
            DataType::BOOL
        } else {
            DataType::STR
        }
    }

    fn infer_metadata(filename: &str) -> (Vec<String>, Vec<DataType>) {
        let mut iter = read_lines(&filename).unwrap();
        let mut colnames: Vec<String> = vec![];
        let mut coltypes: Vec<DataType> = vec![];
        let mut first_row = true;

        while let Some(line) = iter.next() {
            let cols: Vec<String> =
                line.unwrap().split(',').map(|e| e.to_owned()).collect();
            if colnames.len() == 0 {
                colnames = cols;
            } else {
                for (ix, col) in cols.iter().enumerate() {
                    let datatype = CSVNode::infer_datatype(col);
                    if first_row {
                        coltypes.push(datatype)
                    } else if coltypes[ix] != DataType::STR {
                        coltypes[ix] = datatype;
                    } else {
                        coltypes[ix] = DataType::STR;
                    }
                }
                first_row = false;
            }
        }
        //dbg!(&colnames);
        //dbg!(&coltypes);
        (colnames, coltypes)
    }
}

impl CSVNode {
    fn desc(&self, supernode: &Node) -> String {
        let filename =
            self.filename.split("/").last().unwrap_or(&self.filename);

        format!(
            "CSVNode-#{} (p={})|{} {:?}",
            supernode.id(),
            supernode.npartitions(),
            filename,
            self.colnames
        )
        .replace("\"", "\\\"")
    }

    fn next(
        &self, supernode: &Node, flow: &Flow, stage: &Stage, task: &mut Task,
        is_head: bool,
    ) -> Option<Row> {
        let partition_id = task.partition_id;
        let runtime =
            task.contexts.entry(supernode.id()).or_insert_with(|| {
                let partition = &self.partitions[partition_id];
                let mut iter = CSVPartitionIter::new(&self.filename, partition);
                if partition_id == 0 {
                    iter.next(); // Consume the header row
                }
                NodeRuntime::CSV { iter }
            });

        if let NodeRuntime::CSV { iter } = runtime {
            if let Some(line) = iter.next() {
                debug!("line = :{}:", &line.trim_end());
                let cols = line
                    .trim_end()
                    .split(',')
                    .enumerate()
                    .map(|(ix, col)| match self.coltypes[ix] {
                        DataType::INT => {
                            let ival = col.parse::<isize>().unwrap();
                            Datum::INT(ival)
                        }
                        DataType::STR => Datum::STR(Box::new(col.to_owned())),
                        _ => unimplemented!(),
                    })
                    .collect::<Vec<Datum>>();
                return Some(Row::from(cols));
            } else {
                return None;
            }
        }
        panic!("Cannot get NodeRuntime::CSV")
    }
}

/***************************************************************************************************/
#[derive(Debug, Serialize)]
struct ProjectNode {
    colids: Vec<ColId>,
}

impl ProjectNode {
    fn new(colids: Vec<ColId>) -> NodeEnum {
        NodeEnum::ProjectNode(ProjectNode { colids })
    }

    fn desc(&self, supernode: &Node) -> String {
        format!("ProjectNode-#{}|{:?}", supernode.id(), self.colids)
    }

    fn next(
        &self, supernode: &Node, flow: &Flow, stage: &Stage, task: &mut Task,
        is_head: bool,
    ) -> Option<Row> {
        //let flow = task.stage().flow();
        //let flow = &*(&*task.stage).flow;

        if let Some(row) =
            supernode.child(flow, 0).next(flow, stage, task, false)
        {
            return Some(row.project(&self.colids));
        } else {
            return None;
        }
    }
}

impl ProjectNode {}

/***************************************************************************************************/
#[derive(Debug, Serialize)]
struct FilterNode {
    expr: Expr,
}

impl FilterNode {
    fn desc(&self, supernode: &Node) -> String {
        let s = format!("FilterNode-#{}|{}", supernode.id(), self.expr);
        htmlify(s)
    }

    fn next(
        &self, supernode: &Node, flow: &Flow, stage: &Stage, task: &mut Task,
        is_head: bool,
    ) -> Option<Row> {
        while let Some(e) =
            supernode.child(flow, 0).next(flow, stage, task, false)
        {
            if let Datum::BOOL(b) = self.expr.eval(&e) {
                if b {
                    return Some(e);
                }
            }
        }
        return None;
    }
}

impl FilterNode {
    fn new(expr: Expr) -> NodeEnum {
        if let Expr::RelExpr(..) = expr {
            NodeEnum::FilterNode(FilterNode { expr })
        } else {
            panic!("Invalid filter expression")
        }
    }
}

/***************************************************************************************************/
#[derive(Debug, Serialize)]
struct JoinNode {
    preds: Vec<JoinPredicate>, // (left-column,[eq],right-column)*
}

type JoinPredicate = (ColId, RelOp, ColId);

impl JoinNode {
    fn new(preds: Vec<JoinPredicate>) -> NodeEnum {
        NodeEnum::JoinNode(JoinNode { preds })
    }

    fn desc(&self, supernode: &Node) -> String {
        let s = format!("JoinNode-#{}|{:?}", supernode.id(), self.preds);
        htmlify(s)
    }

    fn next(
        &self, supernode: &Node, flow: &Flow, stage: &Stage, task: &mut Task,
        is_head: bool,
    ) -> Option<Row> {
        None
    }
}

impl JoinNode {}

/***************************************************************************************************/
#[derive(Debug, Serialize)]
struct AggNode {
    keycolids: Vec<ColId>,
    aggcolids: Vec<(AggType, ColId)>,
}

impl AggNode {
    fn new(
        keycolids: Vec<ColId>, aggcolids: Vec<(AggType, ColId)>,
    ) -> NodeEnum {
        NodeEnum::AggNode(AggNode {
            keycolids,
            aggcolids,
        })
    }

    fn desc(&self, supernode: &Node) -> String {
        let s = format!(
            "AggNode-#{} (p={})|by = {:?}, aggs = {:?}",
            supernode.id(),
            supernode.npartitions(),
            self.keycolids,
            self.aggcolids
        );
        s
    }

    fn next(
        &self, supernode: &Node, flow: &Flow, stage: &Stage, task: &mut Task,
        is_head: bool,
    ) -> Option<Row> {
        let htable: HashMap<Row, Row> =
            self.run_producer(supernode, flow, stage, task);
        None
    }
}

use std::cmp::Ordering;

impl AggNode {
    fn run_producer_one_row(&self, accrow: &mut Row, currow: &Row) {
        for (ix, &(agg_type, agg_colid)) in self.aggcolids.iter().enumerate() {
            let acccol = accrow.get_column_mut(ix);
            let curcol = currow.get_column(agg_colid);

            match agg_type {
                AggType::COUNT => {
                    *acccol = Datum::INT(acccol.as_int() + 1);
                }
                AggType::SUM => {
                    *acccol = Datum::INT(acccol.as_int() + curcol.as_int());
                }
                AggType::MIN => {
                    if curcol.cmp(&acccol) == Ordering::Less {
                        accrow.set_column(ix, &curcol)
                    }
                }
                AggType::MAX => {
                    if curcol.cmp(&acccol) == Ordering::Greater {
                        accrow.set_column(ix, &curcol)
                    }
                }
            }
        }
    }

    fn run_producer(
        &self, supernode: &Node, flow: &Flow, stage: &Stage, task: &mut Task,
    ) -> HashMap<Row, Row> {
        let mut htable: HashMap<Row, Row> = HashMap::new();
        let child = supernode.child(&*flow, 0);

        while let Some(currow) = child.next(flow, stage, task, false) {
            // build key
            let key = currow.project(&self.keycolids);
            //debug!("-- key = {}", key);

            let acc = htable.entry(key).or_insert_with(|| {
                let acc_cols: Vec<Datum> = self
                    .aggcolids
                    .iter()
                    .map(|&(aggtype, ix)| {
                        // Build an empty accumumator Row
                        match aggtype {
                            AggType::COUNT => Datum::INT(0),
                            AggType::SUM => Datum::INT(0),
                            AggType::MAX | AggType::MIN => {
                                currow.get_column(ix).clone()
                            }
                        }
                    })
                    .collect();
                Row::from(acc_cols)
            });
            AggNode::run_producer_one_row(self, acc, &currow);
            //debug!("   acc = {}", acc);
        }
        for (k, v) in htable.iter() {
            write_partition(flow, stage, task, v);
        }
        htable
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum AggType {
    COUNT,
    MIN,
    MAX,
    SUM,
    //AVG,
}

/***************************************************************************************************/
#[derive(Debug, Serialize)]
pub struct Flow {
    pub nodes: Vec<Node>,
}


impl Flow {
    pub fn get_node(&self, node_id: NodeId) -> &Node {
        &self.nodes[node_id]
    }
}

/***************************************************************************************************/
pub fn make_join_flow() -> Flow {
    let arena: NodeArena = Arena::new();
    let empfilename = format!("{}/{}", DATADIR, "emp.csv").to_string();
    let deptfilename = format!("{}/{}", DATADIR, "dept.csv").to_string();

    let emp = CSVNode::new(&arena, empfilename, 4)
        .project(&arena, vec![0, 1, 2])
        .agg(&arena, vec![0], vec![(AggType::COUNT, 1)], 3);

    let dept = CSVNode::new(&arena, deptfilename, 4);

    let join = emp.join(&arena, vec![&dept], vec![(2, RelOp::Eq, 0)]).agg(
        &arena,
        vec![0],
        vec![(AggType::COUNT, 1)],
        3,
    );
    Flow {
        nodes: arena.into_vec(),
    }
}

fn make_mvp_flow() -> Flow {
    let arena: Arena<_> = Arena::new();

    /*
        CSV -> Project -> Agg
    */
    let csvfilename = format!("{}/{}", DATADIR, "emp.csv");
    let ab = CSVNode::new(&arena, csvfilename.to_string(), 4)
        .project(&arena, vec![2])
        .agg(&arena, vec![0], vec![(AggType::COUNT, 0)], 3);

    Flow {
        nodes: arena.into_vec(),
    }
}

pub fn make_simple_flow() -> Flow {
    let arena: NodeArena = Arena::new();
    let expr = RelExpr(
        Box::new(CID(1)),
        RelOp::Gt,
        Box::new(Literal(Datum::INT(15))),
    );

    let csvfilename = format!("{}/{}", DATADIR, "emp.csv");
    let ab = CSVNode::new(&arena, csvfilename.to_string(), 4) // name,age,dept_id
        //.filter(&arena, expr)
        //.project(&arena, vec![2, 1, 0])
        .agg(
            &arena,
            vec![2],
            vec![
                (AggType::COUNT, 0),
                (AggType::SUM, 2),
                (AggType::MIN, 0),
                (AggType::MAX, 0),
            ],
            3,
        )
        .emit(&arena);

    Flow {
        nodes: arena.into_vec(),
    }
}

/***************************************************************************************************/

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

fn calculate_hash<T: Hash>(t: &T) -> u64 {
    let mut s = DefaultHasher::new();
    t.hash(&mut s);
    s.finish()
}

fn write_partition(flow: &Flow, stage: &Stage, task: &Task, row: &Row) {
    // Key: flow-id / rdd-id / dest-part / src-part
    let npartitions_consumer = stage.npartitions_consumer;
    let partition = calculate_hash(row) % stage.npartitions_consumer as u64;

    debug!("write = {} to partition {} ", row, partition);
}
