use std::cmp::min;
use std::fmt::Formatter;
use std::fs::File;
use std::ops::{AddAssign, Index, IndexMut};
use std::time;

use fixedbitset::FixedBitSet;
use log::{info, debug, warn};
use ndarray::AssignElem;
use ndarray::prelude::*;
use num_traits::{FromPrimitive, Num, ToPrimitive};
use num_traits::NumAssignOps;
use num_traits::real::Real;
use petgraph::csr::Csr;
use rand::distributions::{Standard, Uniform};
use rand::prelude::*;
use rand_xoshiro::Xoshiro256PlusPlus;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sprs::{CsMat, DenseVector, TriMat};
use itertools::Itertools;

use tamc_core::ensembles as ens;
use tamc_core::metropolis::MetropolisSampler;
use tamc_core::parallel::ensembles as pens;
use tamc_core::parallel::pt as ppt;
use tamc_core::pt as pt;
use tamc_core::sa::geometric_beta_schedule;
use tamc_core::traits::*;

use crate::{Instance, State};
use crate::util::{read_adjacency_list_from_file, read_txt_vec};
use tamc_core::pt::PTState;
use crate::ising::BetaOptions::Arr;
use tamc_core::util::monotonic_divisions;

pub type Spin=i8;

#[derive(Debug, Clone, Serialize)]
#[derive()]
pub struct IsingState{
    pub arr: Vec<Spin>,
    #[serde(skip)]
    pub energy: f32,
    energy_init: bool
}

impl IsingState{
    /// Fast access and convert to f64 spin by simply checking the sign
    #[inline]
    pub unsafe fn uget_f64(&self, index: u32) -> f64{
        let &si = self.arr.get_unchecked(index as usize);
        if si > 0{
            1.0
        } else {
            -1.0
        }
    }
    /// Fast access and convert to f64 spin by simply checking the sign
    #[inline]
    pub unsafe fn uget(&self, index: u32) -> i8{
        return *self.arr.get_unchecked(index as usize);
    }
    /// Access and flip the sign of the spin
    #[inline]
    pub unsafe fn uset_neg(&mut self, index: u32){
        *self.arr.get_unchecked_mut(index as usize) *= -1;
    }

    pub fn mag(&self) -> i64{
        let mut m : i64 = 0;
        for &s in self.arr.iter(){
            m += s as i64;
        }
        return m;
    }

    pub fn as_binary_string(&self) -> String{
        let mut s = std::string::String::with_capacity(self.arr.len());
        for &si in self.arr.iter(){
            if si > 0 { // match +1 to 0
                s.push('0');
            } else {  // match -1 to 1
                s.push('1');
            }
        }
        return s;
    }

    pub fn as_bytes(&self) -> Vec<u8>{
        let n = self.arr.len();
        let num_bytes = n/8 + (if n%8 == 0{ 0 } else { 1 });
        let mut bytes_vec: Vec<u8> =(&[0]).repeat(num_bytes);
        for (i, &si) in self.arr.iter().enumerate(){
            let bi = i / 8;
            let k = i % 8;
            unsafe {
                let b = bytes_vec.get_unchecked_mut(bi);
                *b |= ( if si > 0 { 0 } else {1 << k});
            }
        };

        return bytes_vec;
    }

    pub fn as_u64_vec(&self) -> Vec<u64>{
        let n = self.arr.len();
        let num_bytes = n/64 + (if n%64 == 0{ 0 } else { 1 });
        let mut bytes_vec: Vec<u64> =(&[0]).repeat(num_bytes);
        for (i, &si) in self.arr.iter().enumerate(){
            let bi = i / 64;
            let k = i % 64;
            unsafe {
                let b = bytes_vec.get_unchecked_mut(bi);
                *b |= ( if si > 0 { 0 } else {1 << k});
            }
        };

        return bytes_vec;
    }

    pub fn overlap(&self, other: &IsingState) -> i64 {
        let mut q : i64 = 0;
        for (&si, &sj) in self.arr.iter().zip_eq(other.arr.iter()){
            q +=  (si * sj) as i64;
        }
        return q;
    }
}

pub fn rand_ising_state<I: Instance<u32, IsingState>, Rn: Rng+?Sized>(n: u32, instance: &I, rng: &mut Rn) -> IsingState{
    let mut arr = Vec::new();
    arr.reserve(n as usize);
    for _ in 0..n{
        arr.push( 2*rng.sample(Uniform::new_inclusive(0, 1)) - 1);
    }
    let mut ising_state = IsingState{arr, energy: 0.0, energy_init: false};
    instance.energy(&mut ising_state);
    return ising_state;
}

impl Index<usize> for IsingState{
    type Output = Spin;

    fn index(&self, index: usize) -> &Spin {
        return &self.arr[index];
    }
}

impl IndexMut<usize> for IsingState{
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        return &mut self.arr[index]
    }
}

impl State<u32> for IsingState{
    fn accept_move(&mut self, mv: u32) {
        unsafe{
            self.uset_neg(mv);
        }
    }
}

pub struct IsingSampler<'a>{
    pub samp: MetropolisSampler<'a, f32, u32, IsingState, BqmIsingInstance, Uniform<u32>>
}

impl<'a> IsingSampler<'a>{
    pub fn new(instance: &'a BqmIsingInstance, beta: f32, n: u32) -> Self{
        let samp = MetropolisSampler::new_uniform(instance, beta, n);
        return Self{samp};
    }
}

impl<'a, Rn: Rng+?Sized> Sampler<Rn>
for IsingSampler<'a>
    where
{
    type SampleType = IsingState;
    //type ParamType = I::Param;

    fn advance(&self, state: &mut IsingState, rng: &mut Rn) {
        let mv = rng.sample(&self.samp.rand_distr);
        let de = self.samp.advance_impl(mv, state, rng);
        state.energy += de.unwrap_or(0.0);
    }

    fn sweep(&self, state: &mut IsingState, rng: &mut Rn){
        let mut de = 0.0;
        let n = state.arr.len() as u32;
        for i in 0..n{
            let dei = self.samp.advance_impl(i, state, rng);
            de += dei.unwrap_or(0.0);
        }
        state.energy += de;
    }
}


impl<'a> Macrostate<f32>
for IsingSampler<'a>{
    type Microstate = IsingState;

    fn beta(&self) -> f32 {
        return self.samp.beta();
    }

    fn energy(&self, st: &mut IsingState) -> f32 {
        return self.samp.energy(st);
    }
}


/// An Ising instance specified by an arbitrary binary quadratic model
/// in sparse matrix form.
/// The energy function is the Hamiltonian
///     $$ H = \sum_i h_i s_i + \sum_{i<j} J_{ij} s_i s_j $$
///
/// where $h_i$ are the biases and $J_{ij}$ are the couplings
pub struct BqmIsingInstance{
    pub offset: f32,
    pub bias: Vec<f32>,
    pub coupling: CsMat<f32>,
    pub coupling_vecs: Vec<Vec<(u32, f32)>>,
    pub suscept_coefs: Vec<Vec<f64>>
}

impl BqmIsingInstance{
    pub fn new_zero_bias(coupling: CsMat<f32>) -> Self{
        let (n1, n2) = coupling.shape();
        if n1 != n2{
            panic!("couplings matrix must be square, but has shape {}, {}",n1, n2);
        }
        let mut coupling_vecs = Vec::new();
        coupling_vecs.resize(n1, Vec::new());
        for (i, row)in coupling.outer_iterator().enumerate(){
            for (j, &K) in row.iter() {
                if i == j{
                    panic!("Expected a zero-bias Csr instance");
                }
                coupling_vecs[i].push((j.to_u32().unwrap(), K as f32));
            }
        }
        let mut bias = Vec::new();
        bias.resize(n1, 0.0);

        return Self{offset: 0.0, bias, coupling, coupling_vecs, suscept_coefs: Vec::new()};
    }
    pub fn from_instance_file(file: &str, qubo: bool) -> Self{
        let adj_list = read_adjacency_list_from_file(file)
            .expect("Unable to read adjancency from instance file");
        let n = adj_list.len();
        let mut offset = 0.0;
        let mut tri_mat = TriMat::new((n, n));
        let mut coupling_vecs = Vec::with_capacity(n);
        coupling_vecs.resize(n, Vec::new());
        let mut bias = Vec::new();
        bias.resize(n, 0.0);

        for i in 0..n{
            let neighborhood = &adj_list[i];
            coupling_vecs[i].reserve(neighborhood.len());
            for (&j, &K) in neighborhood.iter(){
                if qubo{
                    if i != j {
                        offset += K / 8.0;
                        tri_mat.add_triplet(i, j, K/4.0);
                        coupling_vecs[i].push((j.to_u32().unwrap(), (K/4.0)));
                        bias[i] += K / 4.0;
                    } else {
                        offset += K / 2.0;
                        bias[i] += K / 2.0;
                        coupling_vecs[i].push((j.to_u32().unwrap(), (K/2.0)));
                    }
                } else {
                    if i != j {
                        tri_mat.add_triplet(i, j, K);
                        coupling_vecs[i].push((j.to_u32().unwrap(), K));
                    } else {
                        bias[i] = K;
                    }
                }
            }
        }
        let coupling = tri_mat.to_csr();
        return Self{offset, bias, coupling, coupling_vecs, suscept_coefs: Vec::new() };
    }
    pub fn with_suscept(self, suscept_files: &Vec<String>) -> Self{
        let mut me = self;
        for file in suscept_files.iter(){
            let f = File::open(file).expect("Unable to open susceptibility file");
            let dvec = read_txt_vec(f)
                .expect("Unable to read susceptibility coefficients from file");
            let n1 = dvec.len();
            let n2 = me.bias.len();
            if n1 != n2{
                println!("WARNING: Ignoring suscept file {} - Expected {} coefficients, but got {}",
                         file, n2, n1)
            }
            me.suscept_coefs.push(dvec)
        }
        return me;
    }

    pub fn to_csr_graph(&self) -> Csr<(), ()>{
        // Construct csr graph
        let edges: Vec<_> = self.coupling.iter()
            .map(|(_, (i,j))| (i as u32,j as u32)).collect();
        let g: Csr<(), ()> = Csr::from_sorted_edges(&edges).unwrap();

        return g;
    }

    pub fn suscept(&self, overlap: &[Spin], i: usize) -> f64{
        let mut chi = 0.0;
        for (&w, &si) in self.suscept_coefs[i].iter().zip_eq(overlap.iter()){
            chi += w * (si as f64);
        }
        return chi;
    }
}
impl Instance<u32, IsingState> for BqmIsingInstance {
    type Energy = f32;

    fn energy_ref(&self, state: & IsingState) -> Self::Energy {
        let mut total_energy = self.offset;
        for ( row, i)in self.coupling_vecs.iter().zip(0..){
            unsafe {
                let mut h = 0.0;
                let si = state.uget(i) as Self::Energy;
                h += *self.bias.get_unchecked(i as usize) ;
                for &(j, K) in row.iter() {
                    let sj = state.uget(j) as Self::Energy;
                    h += (K * sj) / 2.0;
                }
                total_energy += h * si;
            }
        }

        return total_energy;
    }
    fn energy(&self, state: &mut IsingState) -> Self::Energy {
        if state.energy_init{
            return state.energy;
        }
        let total_energy = self.energy_ref(state);
        state.energy = total_energy;
        state.energy_init = true;
        return total_energy;
    }

    /// The \Delta E of a move proposal to flip spin i is
    ///   H(-s_i) - H(s_i) = -2 h_i s_i - 2 \sum_j J_{ij} s_i s_j
    /// Safe only if the move is within the size of the state
    unsafe fn delta_energy(&self, state: &mut IsingState, mv: &u32) -> Self::Energy {
        let mut delta_e = 0.0;
        let i = *mv;
        delta_e += *self.bias.get_unchecked(i as usize);
        let row = self.coupling_vecs.get_unchecked(i as usize);
        for &(j, K) in row.iter(){
            delta_e  += K * (state.uget(j) as Self::Energy);
        }
        let si = state.uget(i) as Self::Energy;
        delta_e *= -2.0 * si;

        return delta_e;
    }

    fn size(&self) -> usize {
        return self.bias.len();
    }
}


fn houdayer_cluster_move<R: Rng+?Sized>(replica1: &mut IsingState, replica2: &mut IsingState,
                                        graph: &Csr<(), ()>, rng: &mut R) -> Option<FixedBitSet>{
    use rand::seq::SliceRandom;
    use petgraph::visit::Bfs;
    use petgraph::visit::NodeFiltered;
    //let n = instance.size();
    let n = graph.node_count();
    if n  > u32::MAX as usize{
        panic!("houdayer_cluster_move: instance size must fit in u32")
    }
    let s1 = ArrayView1::from(&replica1.arr);
    let s2 = ArrayView1::from(&replica2.arr);
    let overlap: Array1<i8> = &s1 * &s2;

    // Select a random spin with q=-1
    let mut idxs: Vec<usize> = Vec::new();
    idxs.reserve(n);
    for (i, &qi) in overlap.iter().enumerate(){
        if qi < 0 {
            idxs.push(i);
        }
    }
    let init_spin = idxs.choose(rng);

    let init_spin = match init_spin{
        None => return None, // No spin has q=-1
        Some(&k) => k as u32
    };

    let filtered_graph = NodeFiltered::from_fn(graph, |n|overlap[n as usize] < 0);
    let mut bfs = Bfs::new(&filtered_graph, init_spin);

    while let Some(x) = bfs.next(&filtered_graph){
        ()
    }
    let nodes = bfs.discovered;
    let cluster_size = nodes.count_ones(..);
    //println!("cluster size = {}", cluster_size);
    // Finally, swap all
    for i in nodes.ones(){
        unsafe { std::mem::swap(replica1.arr.get_unchecked_mut(i),  replica2.arr.get_unchecked_mut(i)); }
    }
    // Invalidate energy caches
    replica1.energy_init=false;
    replica2.energy_init=false;
    return Some(nodes);
}

#[derive(Debug, Clone)]
pub struct PtError{
    msg: String
}

impl PtError{
    pub fn new(msg: &str) -> Self{
        Self{msg: msg.to_string()}
    }
}
impl std::fmt::Display for PtError{
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}
impl std::error::Error for PtError { }

#[derive(Clone, Serialize, Deserialize)]
pub struct PtIcmMinResults{
    pub params: PtIcmParams,
    pub timing: f64,
    pub gs_time_steps: Vec<u32>,
    pub gs_energies: Vec<f32>,
    pub gs_states: Vec<Vec<u64>>,
    pub num_measurements: u32,
    pub instance_size: u32,
    pub acceptance_counts: Vec<u32>,
    //pub final_state: Vec<PTState<IsingState>>
}

impl PtIcmMinResults{
    fn new(params: PtIcmParams, num_betas: u32, instance_size: u32) -> Self{
        let acceptance_counts = Array1::zeros(num_betas as usize).into_raw_vec();
        return Self{
            params,
            gs_states: Vec::new(),
            gs_energies: Vec::new(),
            gs_time_steps: Vec::new(),
            num_measurements: 0,
            acceptance_counts,
            timing: 0.0,
            instance_size
            //final_state: Vec::new()
        };
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PtIcmThermalSamples{
    compression_level: u8,
    pub instance_size: u64,
    pub beta_arr: Vec<f32>,
    pub samples: Vec<Vec<Vec<u8>>>,
    pub e: Vec<Vec<f32>>,
    pub q: Vec<Vec<i32>>,
    pub suscept: Vec<Vec<Vec<f32>>>
}

impl PtIcmThermalSamples{
    fn new(beta_arr: &Vec<f32>, instance_size: u64, capacity: usize, samp_capacity: usize,
           nchi: u32,
           compression_level: u8) -> Self{
        let num_betas = beta_arr.len();
        let beta_arr = beta_arr.iter().map(|&x|x as f32).collect();
        let mut me = Self{
            samples: Vec::with_capacity(num_betas),
            beta_arr,
            instance_size,
            e: Vec::with_capacity(num_betas),
            q: Vec::with_capacity(num_betas),
            suscept: Vec::with_capacity(num_betas),
            compression_level
        };
        for _ in 0..num_betas {
            me.e.push(Vec::with_capacity(2*capacity));
            me.q.push(Vec::with_capacity(capacity));
            me.suscept.push(Vec::new());
        }
        for i in 0..num_betas{
            me.suscept[i].resize(nchi as usize, Vec::new());
            for j in 0..nchi{
                me.suscept[i][j as usize].reserve(capacity);
            }
        }
        if compression_level == 0{ // no compression: save all states at all temperatures
            for _ in 0..num_betas {
                me.samples.push(Vec::with_capacity(samp_capacity));
            }
        } else if compression_level == 1{ // save only the lower half of the temperatures
            for _ in 0..(num_betas/2) {
                me.samples.push(Vec::with_capacity(samp_capacity));
            }
        } else { //save only the lowest temperature samples
            me.samples.push(Vec::with_capacity(samp_capacity));
        }
        return me;
    }

    fn measure(&mut self, pt_state: &mut Vec<pt::PTState<IsingState>>, instance:& BqmIsingInstance) {
        let num_chains = pt_state.len();
        let num_betas = pt_state[0].states.len();
        let n = pt_state[0].states[0].arr.len();
        let nchi = instance.suscept_coefs.len();
        let mut overlap_vec : Vec<i8> = Vec::new();
        if nchi > 0 {
            overlap_vec.resize(n, 0);
        }

        for i in 0..num_betas{
            for j in 0..num_chains{
                let isn = &mut pt_state[j].states[i];
                let e = instance.energy(isn);
                self.e[i].push(e as f32);
            }
            for j in 0..(num_chains/2) {
                let isn1 = &pt_state[2*j].states[i];
                let isn2 = &pt_state[2*j+1].states[i];
                if nchi > 0{
                    for (qi,(&s1, &s2)) in overlap_vec.iter_mut().zip_eq(
                        isn1.arr.iter().zip_eq(isn2.arr.iter())){
                        *qi = s1 * s2;
                    }
                    for k in 0..nchi{
                        let chi = instance.suscept(&overlap_vec, k);
                        self.suscept[i][k].push(chi as f32);
                    }
                }

                let q = isn1.overlap(isn2);
                self.q[i].push(q as i32);
            }
        }
    }
    fn sample_states(&mut self, pt_state: & Vec<pt::PTState<IsingState>>) {
        let num_chains = pt_state.len();
        let num_betas = pt_state[0].states.len();
        if self.compression_level == 0 {
            for i in 0..num_betas {
                for j in 0..num_chains {
                    let isn = &pt_state[j].states[i];
                    self.samples[i].push(isn.as_bytes());
                }
            }
        } else if self.compression_level == 1 {
            for i in 0..(num_betas/2){
                let isamp = num_betas - i - 1;
                for j in 0..num_chains {
                    let isn = &pt_state[j].states[isamp];
                    self.samples[i].push(isn.as_bytes());
                }
            }
        } else {
            for j in 0..num_chains {
                let isn = &pt_state[j].states[num_betas - 1];
                self.samples[0].push(isn.as_bytes());
            }
        }
    }
}

#[derive(Copy, Clone, Serialize, Deserialize)]
pub struct BetaSpec{
    pub beta_min: f32,
    pub beta_max: f32,
    pub num_beta: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum BetaOptions{
    Geometric(BetaSpec),
    Arr(Vec<f32>)
}

impl BetaOptions{
    pub fn new_geometric(beta_min: f32, beta_max: f32, num_beta: u32) -> Self{
        return BetaOptions::Geometric(BetaSpec{beta_min, beta_max, num_beta});
    }
    pub fn get_beta_arr(&self) -> Vec<f32>{
        return match &self {
            BetaOptions::Geometric(b) => {
                geometric_beta_schedule(b.beta_min as f64, b.beta_max as f64, b.num_beta as usize)
                    .into_iter().map(|x| x as f32).collect()
            }
            BetaOptions::Arr(v) => {
                ToOwned::to_owned(v)
            }
        };
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PtIcmParams {
    pub num_sweeps: u32,
    pub warmup_fraction: f64,
    pub beta: BetaOptions,
    pub lo_beta: f32,
    pub icm: bool,
    pub num_replica_chains: u32,
    pub threads: u32,
    pub sample: Option<u32>,
    pub sample_states: Option<u32>,
    pub sample_limiting: Option<u8>
}

impl Default for PtIcmParams{
    fn default() -> Self {
        Self{
            num_sweeps: 256,
            warmup_fraction: 0.5,
            beta: BetaOptions::Geometric(BetaSpec{beta_min:0.1, beta_max:10.0, num_beta: 8}),
            lo_beta: 1.0,
            icm: true,
            num_replica_chains: 2,
            threads: 1,
            sample: Some(32),
            sample_states: Some(64),
            sample_limiting: Some(0)
        }
    }
}

impl PtIcmParams {
    // pub fn check_options(&self) -> Result<(), PtError>{
    //
    //     return Ok(())
    // }
}
pub struct PtIcmRunner<'a>{
    params: &'a PtIcmParams,
    instance: &'a BqmIsingInstance,
    g: Csr<(), ()>,
    beta_vec: Vec<f32>,
    meas_init: u32,
    lo_beta_idx: usize
}
impl<'a> PtIcmRunner<'a>{
    pub fn new(instance: &'a BqmIsingInstance, params: &'a PtIcmParams) -> Self
    {
        let beta_vec = params.beta.get_beta_arr();
        let num_betas = beta_vec.len();
        let beta_arr = Array1::from_vec(beta_vec.clone());
        let beta_diff : Array1<f32> = beta_arr.slice(s![1..]).to_owned() - beta_arr.slice(s![..-1]);
        if !beta_diff.iter().all(|&x|x>=0.0) {
            panic!("beta array must be non-decreasing")
        }
        debug!("Temperature (beta) array:\n\t {:5.4} ", beta_arr);
        let lo_beta_ref = beta_vec.iter().enumerate().find(|&(_, &b)| b >= params.lo_beta);
        let lo_beta_idx = match lo_beta_ref{
            None => {
                warn!("Note: lo_beta={} is out of bounds. The largest beta value will be assigned.", params.lo_beta);
                num_betas-1
            }
            Some((i, _)) => {
                i
            }
        };
        info!("Number of sweeps: {}", params.num_sweeps);
        if params.icm {
            info!("Using ICM")
        } else{
            info!("ICM Disabled")
        }
        // Construct csr graph
        let edges: Vec<_> = instance.coupling.iter()
                .map(|(_, (i,j))| (i as u32,j as u32)).collect();
        let g: Csr<(), ()> = Csr::from_sorted_edges(&edges).unwrap();

        let meas_init = (params.warmup_fraction * (params.num_sweeps as f64)) as u32;

        return Self{params, instance, beta_vec, g, meas_init, lo_beta_idx};
    }


    pub fn run_parallel(&self) -> (PtIcmMinResults, PtIcmThermalSamples, Vec<PTState<IsingState>>){
        let m = self.params.num_replica_chains;
        let num_betas = self.beta_vec.len();
        // seed and create random number generator
        let mut rngt = thread_rng();
        let mut seed_seq = [0u8; 32];
        rngt.fill_bytes(&mut seed_seq);
        let mut rng = Xoshiro256PlusPlus::from_seed(seed_seq);
        // randomly generate initial states
        let mut pt_state = self.generate_init_state(&mut rng);
        // generate ensemble rngs
        let mut rng_vec = Vec::with_capacity(num_betas);
        for _ in 0..m{
            let mut rng_chain = Vec::with_capacity(num_betas);
            for _ in 0..num_betas{
                rng_chain.push(rng.clone());
                rng.jump()
            }
            rng_vec.push(rng_chain);
        };

        let (mut pt_results, pt_samps) = self.parallel_pt_loop(&mut pt_state, &mut rng_vec);
        self.count_acc(&pt_state, &mut pt_results);
        return (pt_results, pt_samps, pt_state);
    }


    pub fn run(&self, initial_state: Option<Vec<PTState<IsingState>>>) -> (PtIcmMinResults, PtIcmThermalSamples, Vec<PTState<IsingState>>){
        // seed and create random number generator
        let mut rngt = thread_rng();
        let mut seed_seq = [0u8; 32];
        rngt.fill_bytes(&mut seed_seq);
        let mut rng = Xoshiro256PlusPlus::from_seed(seed_seq);
        // randomly generate initial states
        let mut pt_state = match initial_state{
            None => self.generate_init_state(&mut rng),
            Some(st) => { st }
        };
        let (mut pt_results, pt_samps) = self.pt_loop(&mut pt_state, &mut rng);
        self.count_acc(&pt_state, &mut pt_results);
        //pt_results.final_state = pt_state;
        return (pt_results, pt_samps, pt_state);
    }

    fn parallel_pt_loop<Rn: Rng+Send>(
        &self, pt_state: &mut Vec<pt::PTState<IsingState>>,
        rng_vec: &mut Vec<Vec<Rn>>
    ) -> (PtIcmMinResults, PtIcmThermalSamples)
    {
        // Initialize samplers
        let n = self.instance.size();
        let num_betas = self.beta_vec.len();
        let num_sweeps = self.params.num_sweeps;
        let num_chains = self.params.num_replica_chains;
        let samp_capacity = if let &Some(nsamp) = &self.params.sample{
            num_chains * (num_sweeps - self.meas_init) / nsamp
        } else {
            0
        } as usize;
        let state_samp_capacity = if let &Some(nsamp) = &self.params.sample_states{
            num_chains * (num_sweeps - self.meas_init) / nsamp
        } else {
            0
        } as usize;
        let samplers: Vec<_> = self.beta_vec.iter()
            .map(|&b | IsingSampler::new(self.instance,b, n as u32))
            .collect();
        let pt_sampler = ppt::parallel_tempering_sampler(samplers);
        let mut pt_results = PtIcmMinResults::new(self.params.clone(),num_betas as u32, n as u32);
        let mut pt_samps = PtIcmThermalSamples::new(&self.beta_vec, n as u64,samp_capacity,
                                                    state_samp_capacity, self.instance.suscept_coefs.len() as u32,
                                                    self.params.sample_limiting.unwrap_or(0));
        let mut pt_chains_sampler = pens::ThreadedEnsembleSampler::new(pt_sampler);
        let mut minimum_e = None;
        info!("-- PT-ICM begin");
        let start = time::Instant::now();
        for i in 0..num_sweeps{
            self.apply_icm(pt_state, &mut rng_vec[0][0]);
            pt_chains_sampler.sweep(pt_state, rng_vec);
            self.apply_measurements(i, pt_state, &mut minimum_e, &mut pt_results, &mut pt_samps);
        }
        let end = start.elapsed();
        info!("-- PT-ICM Finished");
        info!("Duration: {:5.4} s", end.as_secs_f64());
        pt_results.timing = end.as_micros() as f64;

        return (pt_results, pt_samps);
    }

    fn pt_loop<Rn: Rng>(
        &self, pt_state: &mut Vec<pt::PTState<IsingState>>,
        rng: &mut Rn
    ) -> (PtIcmMinResults, PtIcmThermalSamples)
    {
        // Initialize samplers
        let n = self.instance.size();
        let num_betas = self.beta_vec.len();
        let num_sweeps = self.params.num_sweeps;
        let num_chains = self.params.num_replica_chains;
        let samp_capacity = if let &Some(nsamp) = &self.params.sample{
            num_chains * (num_sweeps - self.meas_init) / nsamp
        } else {
            0
        } as usize;
        let state_samp_capacity = if let &Some(nsamp) = &self.params.sample_states{
           num_chains * (num_sweeps - self.meas_init) / nsamp
        } else {
            0
        } as usize;

        let samplers: Vec<_> = self.beta_vec.iter()
            .map(|&b | IsingSampler::new(self.instance,b, n as u32))
            .collect();
        let pt_sampler = pt::parallel_tempering_sampler(samplers);
        let mut pt_results = PtIcmMinResults::new(self.params.clone(),num_betas as u32, n as u32);

        let mut pt_samps = PtIcmThermalSamples::new(&self.beta_vec, n as u64, samp_capacity,
                                                    state_samp_capacity, self.instance.suscept_coefs.len() as u32,
                                                    self.params.sample_limiting.unwrap_or(0));
        let mut pt_chains_sampler = ens::EnsembleSampler::new(pt_sampler);
        let mut minimum_e = None;
        info!("-- PT-ICM begin");
        let start = time::Instant::now();
        for i in 0..num_sweeps{
            self.apply_icm(pt_state, rng);
            pt_chains_sampler.sweep(pt_state, rng);
            self.apply_measurements(i, pt_state, &mut minimum_e, &mut pt_results, &mut pt_samps);
        }
        let end = start.elapsed();
        info!("-- PT-ICM Finished");
        info!("Duration: {:5.4} s", end.as_secs_f64());
        pt_results.timing = end.as_micros() as f64;

        return (pt_results, pt_samps);
    }

    fn generate_init_state<Rn: Rng+?Sized>(&self, rng: &mut Rn) -> Vec<pt::PTState<IsingState>>{
        // randomly generate initial states
        let n = self.instance.size() as u32;
        let num_betas = self.beta_vec.len();
        let mut pt_state = Vec::new();
        for _ in 0..self.params.num_replica_chains{
            let mut init_states = Vec::with_capacity(num_betas);
            for _ in 0..num_betas{
                init_states.push(rand_ising_state(n, self.instance, rng));
            }
            pt_state.push(pt::PTState::new(init_states));
        }
        return pt_state;
    }

    fn apply_icm<Rn: Rng+?Sized>(&self, pt_state: &mut Vec<pt::PTState<IsingState>>, rng: &mut Rn)
        -> Vec<Option<FixedBitSet>>
    {
        if !self.params.icm{
            return Vec::new();
        }
        // Apply ICM move
        let lo_beta_idx = self.lo_beta_idx;
        let mut icm_vec = Vec::new();
        for pt_pairs in pt_state.chunks_exact_mut(2){
            let (pt0, pt1) = pt_pairs.split_at_mut(1);
            for (replica1, replica2) in pt0[0].states_mut()[lo_beta_idx..].iter_mut()
                .zip(pt1[0].states_mut()[lo_beta_idx..].iter_mut()){
                let icm_cluster = houdayer_cluster_move(
                    replica1, replica2, &self.g, rng);
                icm_vec.push(icm_cluster);
            }
        }
        return icm_vec;
    }

    fn apply_measurements(&self, i: u32, pt_state: &mut Vec<pt::PTState<IsingState>>,
                          minimum_e: &mut Option<f32>, pt_results: &mut PtIcmMinResults,
                          pt_samples: &mut PtIcmThermalSamples)
    {

        if i >= self.meas_init {
            let stp = i-self.meas_init;
            if let Some(samp_steps) = self.params.sample{
                if stp % samp_steps == 0 || i == self.params.num_sweeps-1{
                    pt_samples.measure(pt_state, &self.instance);
                }
            }
            if let Some(state_samp_steps) = self.params.sample_states{
                if stp % state_samp_steps == 0 || i == self.params.num_sweeps-1{
                    pt_samples.sample_states(pt_state);
                }
            }
            // Measure statistics/lowest energy state so far
            let mut min_energies = Vec::with_capacity(pt_state.len());
            for pts in pt_state.iter_mut() {
                let energies : Vec<f32> = pts.states_mut().iter_mut()
                    .map(|st| self.instance.energy(st)).collect();
                let (i1, &e1) = energies.iter().enumerate()
                    .min_by(|&x, &y| x.1.partial_cmp(&y.1).unwrap())
                    .unwrap();
                min_energies.push((i1, e1))
            }

            let (min_e_ch, &(min_idx, min_e)) = min_energies.iter().enumerate()
                .min_by(|&x, &y| x.1.1.partial_cmp(&y.1.1).unwrap())
                .unwrap();
            let chain = pt_state[min_e_ch].states_ref();
            let min_state = &chain[min_idx];

            if minimum_e.map_or(true, |x| min_e < x) {
                *minimum_e = Some(min_e);
                pt_results.gs_states.push(min_state.as_u64_vec());
                pt_results.gs_energies.push(min_e);
                pt_results.gs_time_steps.push(i)
            }
        }
    }

    fn count_acc(&self, pt_state: & Vec<pt::PTState<IsingState>>, pt_results: &mut PtIcmMinResults){
        let mut acceptance_counts = Array1::zeros(self.beta_vec.len());
        for st in pt_state.iter(){
            acceptance_counts += &st.num_acceptances;
        }
        pt_results.acceptance_counts = acceptance_counts.into_raw_vec();
    }

}
pub fn pt_icm_minimize(instance: &BqmIsingInstance,
                       params: &PtIcmParams)
                       -> PtIcmMinResults
{

    println!(" ** Parallel Tempering - ICM **");
    let pticm = PtIcmRunner::new(instance, params);
    return if params.threads > 1 {
        pticm.run_parallel().0
    } else {
        pticm.run(None).0
    }
}


pub fn pt_optimize_beta(
    instances: &Vec<BqmIsingInstance>,
    params: &PtIcmParams,
    num_iters: u32,
) -> (PtIcmParams, Array2<f32>) {
    use interp::interp;
    use tamc_core::util::{StepwiseMeasure, finite_differences, monotonic_bisection};

    let num_instances = instances.len();
    let mut params = params.clone();
    let num_chains = params.num_replica_chains;
    // Anneal the step-size over num_iters
    let alpha_init = 0.2;
    let alpha_end = 0.2;

    let mut init_states = Vec::with_capacity(instances.len());
    init_states.resize(instances.len(), None);
    let init_beta_vec = params.beta.get_beta_arr();
    let nt = init_beta_vec.len();
    // We use a momentum-directed iteration optimizer
    let mut momentum_beta = Array1::from_vec(init_beta_vec[1..nt-1].to_vec());
    let mut tau_hist : Vec<Array1<f32>> = Vec::new();

    for i in 0..num_iters {
        let alpha = alpha_init + (i as f32 / (num_iters-1) as f32) * (alpha_end - alpha_init);
        println!("* Iteration {}.\n* Step Size: {}", i, alpha);

        let beta_vec = params.beta.get_beta_arr();

        let beta_meas = StepwiseMeasure::new(beta_vec.clone());
        let beta_weights = Array1::from_vec(beta_meas.weights.clone());
        let beta_arr = Array1::from_vec(beta_vec.clone());
        // Run PT on the current temperature array on all replicas
        let pticm_vec: Vec<PtIcmRunner> = instances.iter()
            .map(|i| PtIcmRunner::new(i, &params)).collect();
        let results: Vec<Vec<PTState<IsingState>>> = pticm_vec.par_iter().zip_eq(init_states.par_iter())
            .map(|(p, s)| p.run(s.clone()).2).collect();
        // Gather the diffusion histograms for each temperature summed over all replica chains
        // Also evaluate the round trip times
        let mut dif_probs_vec : Vec<Array1<f32>> = Vec::with_capacity(num_instances);
        let mut tau_vec: Vec<f32> = Vec::with_capacity(num_instances);
        for s in results.iter(){
            let dif_hists : Vec<ArrayView2<u32>>= s.iter().map(|ptstate| ptstate.diffusion_hist.view()).collect();
            // number of round trips in all replica chains
            let rts : u32 =  s.iter().map(|ptstate| ptstate.round_trips).sum();
            let rt_per_rep = rts as f32 / ((num_chains * nt as u32 ) as f32 );
            // The typical rount-trip time per replica is  num_sweeps / (N_t * \bar{\tau} )
            let tau = if rts == 0 { 0.0 } else { (params.num_sweeps as f32)/rt_per_rep };
            tau_vec.push(tau);
            let n = dif_hists.len();
            let sh = dif_hists[0].raw_dim();
            let mut sum_dif_hists = Array2::zeros(sh);
            for h in dif_hists.iter() {
                sum_dif_hists += h;
            }
            let sum_dif_hists = sum_dif_hists.map(|&x| x as f32);
            let tots = sum_dif_hists.sum_axis(Axis(1));
            // n_maxbeta / (n_minbeta + n_maxbeta)
            let dif_probs = sum_dif_hists.slice(s![.., 1]).to_owned() / tots;
            dif_probs_vec.push(dif_probs);
        }
        println!("(Peek) diffusion distribution: {:5.4}", &dif_probs_vec[0]);
        let tau_arr = Array1::from_vec(tau_vec);
        println!("Round-trip times (sweeps):\n{}", tau_arr);
        let d_dif_vec : Vec<Array1<f32>> = dif_probs_vec.iter()
            .map(|f| Array1::from_vec(finite_differences(beta_arr.as_slice().unwrap(),
                                                         f.as_slice().unwrap())) )
            .collect();
        let mut weighted_d_dif = Array1::zeros(nt);
        for (&tau, d_dif) in tau_arr.iter().zip(d_dif_vec.iter()){
            weighted_d_dif.scaled_add(tau, d_dif);
        }
        tau_hist.push(tau_arr);
        println!("Weighed df/dT: {:5.4}", weighted_d_dif);
        let unnorm_eta2 : Array1<f32> = weighted_d_dif / &beta_weights;
        let unnorm_eta = unnorm_eta2.map(|&x| f32::sqrt(x.max(0.0)));
        // Trapezoid rule correction
        let unnorm_eta = (unnorm_eta.slice(s![0..-1]).to_owned() + unnorm_eta.slice(s![1..]))/2.0;
        let z = unnorm_eta.sum();
        if z < f32::EPSILON{
            warn!(" ** Insufficient round trips for eta CDF");
            continue;
        }
        let eta_arr : Array1<f32> = &unnorm_eta / z;
        println!("Eta: {:5.4}", eta_arr);
        let eta_vec = eta_arr.into_raw_vec();
        let eta_cdf : Vec<f32>= eta_vec.iter()
            .scan(0.0, |acc, x|{let acc0 = *acc; *acc += x; Some(acc0)})
            .chain(std::iter::once(1.0))
            .collect();
        let eta_cdf_arr = Array1::from_vec(eta_cdf.clone());
        println!("Eta CDF:\n {:5.4}", eta_cdf_arr);
        let &beta_min = &beta_vec[0];
        let &beta_max = &beta_vec[nt -1];
        let eta_fn = |x|{
            let beta = beta_min + x * (beta_max-beta_min);
            interp(&beta_vec, &eta_cdf, beta)
        };
        let xdivs = monotonic_divisions(eta_fn, (nt -1) as u32);
        let xdivs = Array1::from(xdivs);
        let mut beta_divs : Array1<f32> = xdivs*(beta_max - beta_min) + beta_min;
        let calc_beta_divs = beta_divs.clone();
        // Mean of |log10(b_calc/b_current)|
        let mut err = 0.0;
        println!("Calculated beta:\n{:5.4}", beta_divs);
        // Update momentum
        for (bp, &b1) in momentum_beta.iter_mut()
            .zip(calc_beta_divs.iter().skip(1).take(nt-2)){
            *bp = f32::exp(0.85 * (*bp).ln() + 0.15*b1.ln());
        }
        println!("Momentum beta:\n{:5.4}", momentum_beta);

        for (b2, (b1, bp)) in beta_divs.iter_mut().skip(1).take(nt-2)
                .zip(beta_vec.iter().skip(1).take(nt-2).zip(momentum_beta.iter())){
            let b = f32::exp(alpha * bp.ln() + (1.0-alpha)*b1.ln());
            err += f32::abs(b2.log10() - b1.log10());
            *b2 = b
        }
        err /= nt as f32;
        println!("Next beta:\n{:5.4}", beta_divs);
        println!("Mean abs log rel_err: {}", err);

        params.beta = BetaOptions::Arr(beta_divs.into_raw_vec());
        // let f_arr = results.iter()
        //     .map(|res| res.final_state.iter().map(|c|))
        //println!(" Diffusion function");
        init_states = results.into_iter()
            .map(|mut res| {
                for r in res.iter_mut(){ r.reset_tags() };
                Some(res) })
            .collect();
        if err < 2.0e-2 {
            println!(" ** Relative Error converged");
            break;
        }
        //params.beta = BetaOptions::Arr()
    }

    let final_beta = Array1::from_vec(params.beta.get_beta_arr());
    println!("Final temperature array:\n{:6.5}", final_beta);
    let tau_view : Vec<ArrayView1<f32>> = tau_hist.iter().map(|v|v.view()).collect();
    let tau_hist_arr = ndarray::stack(Axis(0), &tau_view).unwrap();

    return (params, tau_hist_arr);
}

#[cfg(test)]
mod tests {
    use ndarray::prelude::Array1;
    use rand::prelude::*;
    use rand_xoshiro::Xoshiro256PlusPlus;
    use sprs::TriMat;

    use tamc_core::metropolis::MetropolisSampler;
    use tamc_core::pt::{parallel_tempering_sampler, PTState};
    use tamc_core::sa::{geometric_beta_schedule, simulated_annealing};
    use tamc_core::traits::*;

    use crate::ising::{BetaOptions, BqmIsingInstance, pt_icm_minimize, PtIcmParams, rand_ising_state};

    fn make_ising_2d_instance(l: usize) -> BqmIsingInstance{
        let n = l*l;
        let mut tri_mat = TriMat::new((n, n));
        for i in 0..l{
            for j in 0..l{
                let q0 = i*l + j;
                let q1 = ((i+1)%l)*l + j;
                let q2 = i*l + (j+1)%l;
                tri_mat.add_triplet(q0, q1, -1.0);
                tri_mat.add_triplet(q1, q0, -1.0);
                tri_mat.add_triplet(q0, q2, -1.0);
                tri_mat.add_triplet(q2, q0, -1.0);
            }
        }

        let instance = BqmIsingInstance::new_zero_bias(tri_mat.to_csr());
        return instance;
    }
    #[test]
    fn test_ising_2d_sa() {
        let l = 8;
        let n: u32 = l*l;
        let ensemble_size= 16;
        let beta0 :f64 = 0.02;
        let betaf :f64 = 2.0;
        let num_sweeps = 200;

        let mut rng = Xoshiro256PlusPlus::seed_from_u64(1234);
        let instance = make_ising_2d_instance(l as usize);

        let mut init_states = Vec::with_capacity(ensemble_size);
        for _ in 0..ensemble_size{
            init_states.push(rand_ising_state(n, &instance, &mut rng));
        }

        let beta_schedule = geometric_beta_schedule(beta0, betaf, num_sweeps);

        //sampler.advance();
        let mut states = simulated_annealing(&instance, init_states, &beta_schedule, &mut rng, |_i, _|{} );
        for st in states.iter_mut(){
            let mz = st.mag();
            let e = instance.energy(st);
            println!("mz = {}", mz);
            println!("e = {}", e)
        }

        println!("Done.")
    }

    #[test]
    fn test_ising_2d_pt(){
        let l = 16;
        let n: u32 = l*l;
        let beta0 :f64 = 0.02;
        let betaf :f64 = 2.0;
        let num_betas = 16;
        let num_sweeps = 200;

        let mut rng = Xoshiro256PlusPlus::seed_from_u64(1234);
        let instance = make_ising_2d_instance(l as usize);
        let mut init_states = Vec::with_capacity(num_betas);
        for _ in 0..num_betas{
            init_states.push(rand_ising_state(n, &instance, &mut rng));
        }
        let betas = geometric_beta_schedule(beta0, betaf, num_betas);
        let samplers: Vec<_> = betas.iter()
                .map(|&b |MetropolisSampler::new_uniform(&instance,b, n as u32))
                .collect();
        let pt_sampler = parallel_tempering_sampler(samplers);
        let mut init_state = PTState::new(init_states);

        let mut pt_icm_params = PtIcmParams::default();
        pt_icm_params.num_sweeps = num_sweeps;
        pt_icm_params.beta = BetaOptions::new_geometric(0.1, 10.0, num_betas as u32);
        let opts_str = serde_yaml::to_string(&pt_icm_params).unwrap();
        println!("{}", opts_str);
        let beta_arr = pt_icm_params.beta.get_beta_arr();
        let pt_results = pt_icm_minimize(&instance,  &pt_icm_params);
        for (&e, &t) in pt_results.gs_energies.iter().zip(pt_results.gs_time_steps.iter()){
            println!("t={}, e = {}", t, e)
        }
        let acc_counts = Array1::from_vec(pt_results.acceptance_counts);
        let acc_prob = acc_counts.map(|&x|(x as f64)/((2*num_sweeps) as f64));
        for (&b, &p) in beta_arr.iter().zip(acc_prob.iter()){
            println!("beta {} : acc_p = {}", b, p)
        }
    }
}
