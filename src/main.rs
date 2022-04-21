//#![allow(unused_variables)]

use std::collections::HashMap;
use std::fs;
use task::ThreadPool;

use crate::includes::*;

#[macro_use]
extern crate lalrpop_util;

#[macro_use]
extern crate fstrings;

lalrpop_mod!(pub sqlparser); // synthesized by LALRPOP

pub mod includes;

pub mod ast;
pub mod bitset;
pub mod expr;
pub mod graph;
pub mod qgm;
pub mod qgmiter;

pub mod compiler;
pub mod lop;
pub mod qst;

pub mod graphviz;
pub mod logging;

pub mod csv;
pub mod flow;
pub mod metadata;
pub mod pop;
pub mod row;
pub mod scratch;
pub mod task;

use ast::*;
use flow::*;
use metadata::*;
use qgm::*;

pub struct Env {
    thread_pool: ThreadPool,
    metadata: Metadata,
    options: HashMap<String, String>,
    input_filename: String,
    output_dir: String,
}

impl Env {
    fn new(nthreads: usize, input_filename: String, output_dir: String) -> Self {
        let thread_pool = task::ThreadPool::new(nthreads);
        let metadata = Metadata::new();
        Env {
            thread_pool,
            metadata,
            options: HashMap::new(),
            input_filename,
            output_dir,
        }
    }

    fn set_option(&mut self, name: String, value: String) {
        debug!("SET {} = {}", &name, &value);
        self.options.insert(name.to_uppercase(), value.to_uppercase());
    }

    fn get_boolean_option(&self, name: &str) -> bool {
        if let Some(opt) = self.options.get(name) {
            return match opt.as_str() {
                "TRUE" | "YES" => true,
                _ => false,
            };
        } else {
            return false;
        }
    }
}

/***************************************************************************************************/
pub fn run_flow(env: &mut Env, flow: &Flow) -> Result<(), String> {
    // Clear output directories
    let dirname = format!("{}/flow", TEMPDIR);
    std::fs::remove_dir_all(dirname).map_err(stringify)?;

    // Run the flow
    flow.run(&env);

    env.thread_pool.close_all();
    env.thread_pool.join();
    Ok(())
}

fn run_job(env: &mut Env) -> Result<(), String> {
    let filename = env.input_filename.as_str();
    let contents = fs::read_to_string(filename).expect(&format!("Cannot open file: {}", &filename));

    let mut parser_state = ParserState::new();

    let qgm_raw_filename = format!("{}/{}", env.output_dir, "qgm_raw.dot");
    let qgm_resolved_filename = format!("{}/{}", env.output_dir, "qgm_resolved.dot");

    // Remove commented lines
    let astlist: Vec<AST> = sqlparser::JobParser::new().parse(&mut parser_state, &contents).unwrap();
    for ast in astlist.into_iter() {
        match ast {
            AST::CatalogTable { name, options } => {
                env.metadata.catalog_table(name, options)?;
            }
            AST::DescribeTable { name } => {
                env.metadata.describe_table(name)?;
            }
            AST::SetOption { name, value } => {
                env.set_option(name, value);
            }
            AST::QGM(mut qgm) => {
                qgm.write_qgm_to_graphviz(&qgm_raw_filename, false)?;
                qgm.resolve(&env)?;
                qgm.write_qgm_to_graphviz(&qgm_resolved_filename, false)?;

                if !env.get_boolean_option("PARSE_ONLY") {
                    let _flow = Flow::compile(env, &mut qgm).unwrap();
                    //run_flow(env, &flow);
                }
            }
        }
    }
    //dbg!(&env.metadata);
    Ok(())
}

/*
********************************** main ****************************************************************
*/
fn main() -> Result<(), String> {
    // Initialize logger with INFO as default
    logging::init("debug");

    let input_filename = "/Users/adarshrp/Projects/flare/sql/spja.fsql".to_string();
    let output_dir = "/Users/adarshrp/Projects/flare/tmp".to_string();
    let mut env = Env::new(1, input_filename, output_dir);

    let jobres = run_job(&mut env);
    if let Err(flare_err) = jobres {
        let errstr = format!("{}", &flare_err);
        error!("{}", errstr);
        return Err(errstr);
    }
    Ok(())
}
/*
********************************** run_unit_tests *********************************************************
*/
#[test]
fn run_unit_tests() -> Result<(), String> {
    use std::process::Command;

    // Initialize logger with INFO as default
    logging::init("error");
    let mut npassed = 0;
    let mut ntotal = 0;

    for test in vec!["rst", "repartition", "groupby", "spja"] {
        let input_filename = f!("/Users/adarshrp/Projects/flare/sql/{test}.fsql");
        let output_dir = f!("/Users/adarshrp/Projects/flare/tests/output/{test}/");

        println!("---------- Running subtest {}", input_filename);
        std::fs::remove_dir_all(&output_dir).map_err(stringify)?;
        std::fs::create_dir_all(&output_dir).map_err(stringify)?;

        ntotal = ntotal + 1;
        let mut env = Env::new(1, input_filename, output_dir.clone());

        let jobres = run_job(&mut env);
        if let Err(flare_err) = jobres {
            let errstr = format!("{}", &flare_err);
            error!("{}", errstr);
        }
        // Compare with gold output
        let gold_dir = f!("/Users/adarshrp/Projects/flare/tests/gold/{test}/");

        let output = Command::new("diff").arg(gold_dir).arg(output_dir).output().expect("failed to execute process");

        let mut mismatch = false;
        for (tag, buf) in vec![("out", output.stdout), ("err", output.stderr)].iter() {
            if buf.len() > 0 {
                mismatch = true;
                let s = String::from_utf8_lossy(buf);
                println!("{}: {}", tag, s);
            }
        }
        if !mismatch {
            npassed = npassed + 1
        }
    }
    println!("---------- Completed: {}/{} subtests passed", npassed, ntotal);
    Ok(())
}
