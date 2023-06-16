// Compile

use crate::{
    bitset::Bitset,
    flow::Flow,
    graph::{ExprKey, Graph, LOPKey, POPKey},
    includes::*,
    lop::{LOPGraph, LOPProps, VirtCol, LOP},
    metadata::{PartType, TableType},
    pcode::PCode,
    pop::{POPGraph, POPProps, Projection, ProjectionMap, POP},
    pop_aggregation,
    pop_csv::CSV,
    pop_hashjoin, pop_repartition,
    qgm::QGM,
    stage::StageGraph,
};
use std::rc::Rc;

impl POP {
    pub fn compile(env: &Env, qgm: &mut QGM, lop_graph: &LOPGraph, lop_key: LOPKey) -> Result<Flow, String> {
        // Build physical plan
        let mut stage_graph = StageGraph::new();

        let root_stage_id = stage_graph.add_stage(lop_key, None);
        let root_pop_key = Self::compile_lop(qgm, &lop_graph, lop_key, &mut stage_graph, root_stage_id)?;
        stage_graph.set_pop_key(root_stage_id, root_pop_key);

        // Diagnostics
        stage_graph.print();

        let plan_pathname = format!("{}/{}", env.output_dir, "pop.dot");
        QGM::write_physical_plan_to_graphviz(qgm, &stage_graph, &plan_pathname)?;

        // Build flow (POPs + Stages)
        let flow = Flow { id: env.id, stage_graph };
        return Ok(flow);
    }

    pub fn compile_lop(qgm: &mut QGM, lop_graph: &LOPGraph, lop_key: LOPKey, stage_graph: &mut StageGraph, stage_id: StageId) -> Result<POPKey, String> {
        let (lop, _, lop_children) = lop_graph.get3(lop_key);

        // Do we have a new stage?
        let effective_stage_id = if matches!(lop, LOP::Repartition { .. }) {
            stage_graph.add_stage(lop_key, Some(stage_id))
        } else {
            stage_id
        };

        // Compile children first
        let mut pop_children = vec![];
        if let Some(lop_children) = lop_children {
            for lop_child_key in lop_children {
                let pop_key = Self::compile_lop(qgm, lop_graph, *lop_child_key, stage_graph, effective_stage_id)?;
                pop_children.push(pop_key);
            }
        }

        // Get schema to write+read repartitioning files to disk (arrow2)
        let schema = if matches!(lop, LOP::Repartition { .. }) {
            Some(Rc::new(lop_key.get_schema(qgm, lop_graph)))
        } else {
            None
        };

        let pop_children_clone = pop_children.clone();

        let pop_key: POPKey = match lop {
            LOP::TableScan { .. } => Self::compile_scan(qgm, lop_graph, lop_key, stage_graph, effective_stage_id)?,
            LOP::HashJoin { .. } => Self::compile_join(qgm, lop_graph, lop_key, stage_graph, effective_stage_id, pop_children)?,
            LOP::Repartition { cpartitions } => Self::compile_repartition_write(
                qgm,
                lop_graph,
                lop_key,
                stage_graph,
                effective_stage_id,
                pop_children,
                schema.clone().unwrap(),
                *cpartitions,
            )?,
            LOP::Aggregation { .. } => Self::compile_aggregation(lop_graph, lop_key, stage_graph, effective_stage_id, pop_children)?,
        };

        debug!("Compiled LOP = {:?} -> POP = {:?} to stage {}", lop_key, pop_key, effective_stage_id);
        debug!("   children =  {:?}", pop_children_clone);

        if stage_id != effective_stage_id {
            stage_graph.set_pop_key(effective_stage_id, pop_key)
        }

        // Add RepartionRead
        if let LOP::Repartition { cpartitions } = lop {
            let pop_key: POPKey = Self::compile_repartition_read(
                lop_graph,
                lop_key,
                stage_graph,
                stage_id,
                effective_stage_id,
                pop_key,
                schema.unwrap(),
                *cpartitions,
            )?;
            debug!("Compiled LOP = {:?} -> POP = {:?} to stage {}", lop_key, pop_key, stage_id);
            return Ok(pop_key);
        }
        Ok(pop_key)
    }

    pub fn compile_scan(qgm: &mut QGM, lop_graph: &LOPGraph, lop_key: LOPKey, stage_graph: &mut StageGraph, stage_id: StageId) -> Result<POPKey, String> {
        let (lop, lopprops, ..) = lop_graph.get3(lop_key);

        let qunid = lopprops.quns.elements()[0];
        let tbldesc = qgm.metadata.get_tabledesc(qunid).unwrap();

        // Build input map
        let (input_projection, input_proj_map) = if let LOP::TableScan { input_projection } = lop {
            let proj_map: ProjectionMap = Self::compute_projection_map(input_projection, None);
            let input_projection = input_projection.elements().iter().map(|&quncol| quncol.1).collect::<Vec<ColId>>();
            (input_projection, proj_map)
        } else {
            return Err(format!("Internal error: compile_scan() received a POP that isn't a TableScan"));
        };
        debug!("Compile input_projection for lopkey {:?}: {:?}", lop_key, input_projection);

        // Compile predicates
        debug!("Compile predicates for lopkey {:?}", lop_key);
        let predicates = Self::compile_predicates(qgm, &lopprops.preds, &input_proj_map);

        let pop = match tbldesc.get_type() {
            TableType::CSV => {
                let inner = CSV::new(
                    tbldesc.pathname().clone(),
                    tbldesc.fields().clone(),
                    tbldesc.header(),
                    tbldesc.separator(),
                    lopprops.partdesc.npartitions,
                    input_projection,
                );
                POP::CSV(inner)
            }
        };

        // Compile real + virt columns
        let (cols, virtcols) = Self::compile_projection(qgm, lop_key, lopprops, &input_proj_map);
        let props = POPProps::new(predicates, cols, virtcols, lopprops.partdesc.npartitions);

        let pop_graph = &mut stage_graph.stages[stage_id].pop_graph;
        let pop_key = pop_graph.add_node_with_props(pop, props, None);

        Ok(pop_key)
    }

    pub fn compile_projection(qgm: &QGM, lop_key: LOPKey, lopprops: &LOPProps, proj_map: &ProjectionMap) -> (Option<Vec<ColId>>, Option<Vec<PCode>>) {
        let cols = lopprops
            .cols
            .elements()
            .iter()
            .map(|&quncol| {
                let prj = Projection::QunCol(quncol);
                proj_map.get(prj).unwrap()
            })
            .collect::<Vec<ColId>>();
        let cols = if cols.len() > 0 { Some(cols) } else { None };

        debug!("Compile real cols for lopkey: {:?} = {:?}", lop_key, cols);

        debug!("Compile virtcols for lopkey: {:?}", lop_key);
        let virtcols = Self::compile_virtcols(qgm, lopprops.virtcols.as_ref(), &proj_map);

        (cols, virtcols)
    }

    pub fn compile_virtcols(qgm: &QGM, virtcols: Option<&Vec<VirtCol>>, proj_map: &ProjectionMap) -> Option<Vec<PCode>> {
        if let Some(virtcols) = virtcols {
            let pcodevec = virtcols
                .iter()
                .map(|expr_key| {
                    let mut pcode = PCode::new();
                    expr_key.compile(&qgm.expr_graph, &mut pcode, &proj_map);
                    pcode
                })
                .collect::<Vec<_>>();
            Some(pcodevec)
        } else {
            None
        }
    }

    pub fn compute_projection_map(cols: &Bitset<QunCol>, virtcols: Option<&Vec<VirtCol>>) -> ProjectionMap {
        let mut proj_map = ProjectionMap::new();

        // Add singleton columns
        for (ix, &quncol) in cols.elements().iter().enumerate() {
            let prj = Projection::QunCol(quncol);
            proj_map.set(prj, ix);
        }

        // Add virtual columns
        if let Some(virtcols) = virtcols {
            let nrealcols = cols.len();
            for (ix, &virtcol) in virtcols.iter().enumerate() {
                let prj = Projection::VirtCol(virtcol);
                proj_map.set(prj, nrealcols + ix);
            }
        }
        proj_map
    }

    pub fn compile_predicates(qgm: &QGM, preds: &Bitset<ExprKey>, proj_map: &ProjectionMap) -> Option<Vec<PCode>> {
        if preds.len() > 0 {
            let exprs = preds.elements();
            Self::compile_exprs(qgm, &exprs, proj_map)
        } else {
            None
        }
    }

    pub fn compile_exprs(qgm: &QGM, exprs: &Vec<ExprKey>, proj_map: &ProjectionMap) -> Option<Vec<PCode>> {
        let mut pcodevec = vec![];
        if exprs.len() > 0 {
            for expr_key in exprs.iter() {
                debug!("Compile expression: {:?}", expr_key.printable(&qgm.expr_graph, false));

                let mut pcode = PCode::new();
                expr_key.compile(&qgm.expr_graph, &mut pcode, &proj_map);
                pcodevec.push(pcode);
            }
            Some(pcodevec)
        } else {
            None
        }
    }

    pub fn compile_repartition_write(
        qgm: &mut QGM, lop_graph: &LOPGraph, lop_key: LOPKey, stage_graph: &mut StageGraph, stage_id: StageId, pop_children: Vec<POPKey>, schema: Rc<Schema>,
        cpartitions: usize,
    ) -> Result<POPKey, String> {
        let (_, lopprops, children) = lop_graph.get3(lop_key);

        // We shouldn't have any predicates
        let predicates = None;
        assert!(lopprops.preds.len() == 0);

        // Build projection map of child. This will be used to resolve any column references in this LOP
        let child_lop_key = children.unwrap()[0];
        let child_lopprops = lop_graph.get_properties(child_lop_key);
        let proj_map: ProjectionMap = Self::compute_projection_map(&child_lopprops.cols, child_lopprops.virtcols.as_ref());

        // Compile real + virt columns
        let (cols, virtcols) = Self::compile_projection(qgm, lop_key, lopprops, &proj_map);
        let props = POPProps::new(predicates, cols, virtcols, lopprops.partdesc.npartitions);

        let repart_key = if let PartType::HASHEXPR(partkey) = &lopprops.partdesc.part_type {
            debug!("Compile pkey start");
            let repart_code = Self::compile_exprs(qgm, partkey, &proj_map).unwrap();
            repart_code
        } else {
            panic!("Invalid partitioning type")
        };
        debug!("Compile pkey end");

        let pop_inner = pop_repartition::RepartitionWrite::new(repart_key, schema, cpartitions);
        let pop_graph = &mut stage_graph.stages[stage_id].pop_graph;
        let pop_key = pop_graph.add_node_with_props(POP::RepartitionWrite(pop_inner), props, Some(pop_children));

        Ok(pop_key)
    }

    pub fn compile_repartition_read(
        lop_graph: &LOPGraph, lop_key: LOPKey, stage_graph: &mut StageGraph, stage_id: StageId, child_stage_id: StageId, child_pop_key: POPKey,
        schema: Rc<Schema>, npartitions: usize,
    ) -> Result<POPKey, String> {
        let lopprops = &lop_graph.get(lop_key).properties;

        // No predicates
        let predicates = None;

        // Compile cols
        let ncols = lopprops.cols.len() + lopprops.virtcols.as_ref().map_or(0, |v| v.len());
        let cols = Some((0..ncols).collect::<Vec<ColId>>());

        // No virtcols
        let virtcols = None;

        let props = POPProps::new(predicates, cols, virtcols, npartitions);

        let pop_inner = pop_repartition::RepartitionRead::new(schema, child_stage_id, child_pop_key);
        let pop_graph = &mut stage_graph.stages[stage_id].pop_graph;

        let pop_key = pop_graph.add_node_with_props(POP::RepartitionRead(pop_inner), props, None);

        Ok(pop_key)
    }

    pub fn compile_join(
        qgm: &mut QGM, lop_graph: &LOPGraph, lop_key: LOPKey, stage_graph: &mut StageGraph, stage_id: StageId, pop_children: Vec<POPKey>,
    ) -> Result<POPKey, String> {
        let (_, lopprops, children) = lop_graph.get3(lop_key);

        // Build projection map of child. This will be used to resolve any column references in this LOP
        let child_lop_key = children.unwrap()[0];
        let child_lopprops = lop_graph.get_properties(child_lop_key);
        let proj_map: ProjectionMap = Self::compute_projection_map(&child_lopprops.cols, child_lopprops.virtcols.as_ref());

        // Compile predicates
        debug!("Compile predicates for lopkey {:?}", lop_key);
        let predicates = Self::compile_predicates(qgm, &lopprops.preds, &proj_map);

        let props = POPProps::new(predicates, None, None, lopprops.partdesc.npartitions);

        let pop_inner = pop_hashjoin::HashJoin {};
        let pop_graph = &mut stage_graph.stages[stage_id].pop_graph;

        let pop_key = pop_graph.add_node_with_props(POP::HashJoin(pop_inner), props, Some(pop_children));

        Ok(pop_key)
    }

    pub fn compile_aggregation(
        lop_graph: &LOPGraph, lop_key: LOPKey, stage_graph: &mut StageGraph, stage_id: StageId, pop_children: Vec<POPKey>,
    ) -> Result<POPKey, String> {
        let lopprops = &lop_graph.get(lop_key).properties;
        let _proj_map: ProjectionMap = Self::compute_projection_map(&lopprops.cols, lopprops.virtcols.as_ref());

        // Compile predicates
        debug!("Compile predicate for lopkey: {:?}", lop_key);
        let predicates = None; // todo Self::compile_predicates(qgm, &lopprops.preds, &proj_map);

        // Compile virtcols
        debug!("Compile virtcols for lopkey: {:?}", lop_key);
        let virtcols = None; // todo Self::compile_virtcols(qgm, lopprops.virtcols.as_ref(), &proj_map);

        let props = POPProps::new(predicates, None, virtcols, lopprops.partdesc.npartitions);

        let pop_inner = pop_aggregation::Aggregation {};
        let pop_graph = &mut stage_graph.stages[stage_id].pop_graph;
        let pop_key = pop_graph.add_node_with_props(POP::Aggregation(pop_inner), props, Some(pop_children));

        Ok(pop_key)
    }
}
