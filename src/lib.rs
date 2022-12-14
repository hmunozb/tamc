//!
//! Trait descriptions:
//!    An *instance* is a specification of a computational problem, including an
//!    energy (i.e. cost function), which may be parametric,
//!    to optimize or sample defined over a sample space.
//!    It must be immutable and thread-safe. It has associated types
//!
//!    A *state* is a mutable point in the sample space of an instance
//!
//!    An *entropy source* is a generalization of an Rng
//!
//!    A *sampler* advances a state using immutable data, an instance and parameters,
//!    along with an entropy source
//!
//!
use structopt::StructOpt;
use num_traits::{Num, Zero};
use num_traits::real::Real;
use rand::Rng;
use rand::distributions::{Distribution, Standard};
use rand::distributions::uniform::{SampleUniform, Uniform};
use std::marker::PhantomData;
use std::error::Error;
use std::ffi::OsStr;
pub use tamc_core::metropolis;
pub use tamc_core::traits::*;
use serde::{Serialize, Deserialize};
pub mod csr;
pub mod util;
pub mod ising;
pub mod percolation;
use std::fs::File;
use crate::ising::PtIcmParams;
use std::fmt;
use std::path::Path;

#[derive(Serialize, Deserialize)]
pub struct PTOptions{
    pub icm: bool,
    pub beta_points: Vec<f64>
}

#[derive(Debug)]
pub enum PTError {
    IoError(std::io::Error, String),
    MethodParse(Box<dyn Error + 'static>, String)
}

impl fmt::Display for PTError{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self{
            PTError::IoError(e, s) => {
                write!(f, "Could not open '{}'", s)
            },
            PTError::MethodParse(e, s) =>{
                writeln!(f, "Failed to parse '{}' as valid input", s)?;
                write!(f, "{}", e.as_ref())
            }
        }
    }
}

impl std::error::Error for PTError{
    fn source(&self) -> Option<&(dyn Error + 'static)>{
        match self{
            PTError::IoError(e, _) => Some(e),
            PTError::MethodParse(e, _) => Some(e.as_ref())
        }
    }
}

#[derive(Serialize, Deserialize)]
pub enum Method{
    PT(PtIcmParams)
}

#[derive(StructOpt)]
pub struct Prog{
    pub method_file: String,
    pub instance_file: String,
    pub output_file: String,
    #[structopt(long)]
    pub suscepts: Vec<String>,
    #[structopt(long)]
    pub sample_output: Option<String>,
    #[structopt(long)]
    pub qubo: bool
}

pub fn run_program(prog: Prog) -> Result<(), Box<dyn Error>>{
    let method_file = prog.method_file;
    let instance_file = prog.instance_file;
    let sample_output = prog.sample_output.unwrap_or("samples.bin".to_string());
    let mut instance = ising::BqmIsingInstance::from_instance_file(&instance_file, prog.qubo);
    if prog.suscepts.len() > 0{
        instance = instance.with_suscept(&prog.suscepts);
    }
    let yaml_str = std::fs::read_to_string(&method_file)
        .map_err(|e| PTError::IoError(e, method_file.to_string() ))?;
    let opts: Method = serde_yaml::from_str(&yaml_str)
        .map_err(|e| PTError::MethodParse(Box::new(e), method_file.to_string()))?;

    match opts{
        Method::PT(pt_params) => {
            //let results = ising::pt_icm_minimize(&instance,&pt_params);
            println!(" ** Parallel Tempering - ICM **");
            let pticm = ising::PtIcmRunner::new(&instance, &pt_params);
            let results = if pt_params.threads > 1 {
                pticm.run_parallel()
            } else {
                pticm.run(None)
            };
            let (gs_results, samp_results, _) = results;
            println!("PT-ICM Done.");
            println!("** Ground state energy **");
            println!("  e = {}", gs_results.gs_energies.last().unwrap());
            {
                let f = File::create(prog.output_file)
                    .expect("Failed to create yaml output file");
                serde_yaml::to_writer(f, &gs_results)
                    .expect("Failed to write to yaml file.")
            }
            {
                let mut f = File::create(&sample_output)
                    .expect("Failed to create sample output file");
                let ext = Path::new(&sample_output).extension().and_then(OsStr::to_str);
                if ext == Some("pkl"){
                    serde_pickle::to_writer(&mut f, &samp_results, serde_pickle::SerOptions::default());
                } else {
                    bincode::serialize_into(&mut f, &samp_results).expect("Failed to serialize");
                }
            }
        }
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
