use super::fri_utils::*;
use crate::circuit::recursive_redshift::oracles::*;

use bellman::pairing::{
    Engine,
};
use bellman::{
    SynthesisError,
    ConstraintSystem,
};

use bellman::pairing::ff::{
    Field,
    PrimeField,
};

use crate::circuit::num::*;
use crate::circuit::boolean::*;


pub struct FriVerifierGadget<E: Engine, I: OracleGadget<E>> {
    pub collapsing_factor : usize,
    //number of iterations done during FRI query phase
    pub num_query_rounds : usize,
    pub initial_degree_plus_one : usize,
    pub lde_factor: usize,
    //the degree of the resulting polynomial at the bottom level of FRI
    pub final_degree_plus_one : usize,

    _engine_marker : std::marker::PhantomData<E>,
    _oracle_marker : std::marker::PhantomData<I>,
}

pub type Label = &'static str;
pub type CombinerFunction<E> = dyn Fn(Vec<Labeled<&Num<E>>>) -> Result<Num<E>, SynthesisError>;

pub struct Labeled<T> {
    pub label: Label,
    pub data: T,
}

pub struct FriSingleQueryRoundData<E: Engine, I: OracleGadget<E>> {   
    upper_layer_queries: Vec<Labeled<Query<E, I>>>,
    // this structure is modified internally as we simplify Nums during he work of the algorithm
    queries: Vec<Query<E, I>>,
    natural_first_element_index : usize,
}


impl<E: Engine, I: OracleGadget<E>> FriVerifierGadget<E, I> {

    fn verify_single_proof_round<CS: ConstraintSystem<E>>(
        
        mut cs: CS,

        upper_layer_queries: &[Labeled<Query<E, I>>],
        upper_layer_commitments: &[Labeled<I::Commitment>], 
        upper_layer_combiner: &CombinerFunction<E>,
        fri_helper: &mut FriUtilsGadget<E>,

        queries: &mut [Query<E, I>],
        commitments: &[I::Commitment],
        final_coefficients: &[AllocatedNum<E>],

        natural_first_element_index: &[Boolean],
        fri_challenges: &[AllocatedNum<E>],
        
        initial_domain_size: usize,
        collapsing_factor: usize,
        oracle_params: &I::Params,
   
    ) -> Result<Boolean, SynthesisError>
    {

        let mut natural_index = &natural_first_element_index[..];
        let coset_idx = fri_helper.get_coset_idx_for_natural_index(natural_index);
        let coset_size = 1 << collapsing_factor;

        // check oracle proof for each element in the upper layer!
        let oracle = I::new(oracle_params);
        let mut final_result = Boolean::Constant(true);

        for labeled_query in upper_layer_queries.iter() {

            let label = &labeled_query.label;
            let commitment_idx = upper_layer_commitments.iter().position(|x| x.label == *label).ok_or(SynthesisError::Unknown)?;
            let commitment = &upper_layer_commitments[commitment_idx].data;
            let oracle_check = oracle.validate(
                cs.namespace(|| "Oracle proof"),
                fri_helper.get_log_domain_size(),
                &labeled_query.data.values, 
                coset_idx,
                commitment, 
                &labeled_query.data.proof, 
            )?;

            final_result = Boolean::and(cs.namespace(|| "and"), &final_result, &oracle_check)?;
        }

        // apply combiner function in order to conduct Fri round consistecy check
        // with respect to the topmost layer
        // let n be the size of coset
        // let the values contained inside queries to be (a_1, a_2, ..., a_n), (b_1, b_2, ..., b_n) , ..., (c_1, ..., c_n)
        // Coset combining function F constructs new vector of length n: (d_1, ..., d_n) via the following_rule : 
        // d_i = F(a_i, b_i, ..., c_i, x_i), i in (0..n)
        // here additiona argument x_i is the evaluation point and is defined by the following rule:
        // if the coset idx has bit representation xxxxxxxx, then x_i = w^(bitreverse(yyyy)|xxxxxxx)
        // here i = |yyyy| (bit decomposition)
        // From this we see that the only common constrained part for all x_i is coset_omega = w^(xxxxxx)
        // and to get corresponding x_i we need to multiply coset_omega by constant c_i = w^(bitreverse(yyyy)|000000)
        // if g = w^(100000) then c_i = w^(bitreverse(yyyy) * 1000000) = g^(bitreverse(yyyy))
        // constants c_i are easily deduced from domain parameters
        // construction of x_i is held by fri_utils

        let mut values = Vec::with_capacity(coset_size);
        let evaluation_points = fri_helper.get_combiner_eval_points(
            cs.namespace(|| "find evaluation points"), 
            coset_idx.iter()
        )?;

        for i in 0..coset_size {

            let mut labeled_argument : Vec<Labeled<&Num<E>>> = upper_layer_queries.iter().map(|x| {
                Labeled {label: x.label, data: &x.data.values[i]}
                }).collect();
            labeled_argument.push(Labeled {
                label: "ev_p",
                data: &evaluation_points[i]
            });

            let res = upper_layer_combiner(labeled_argument)?;
            values.push(res);
        }

        let mut previous_layer_element = fri_helper.coset_interpolation_value(
            cs.namespace(|| "coset interpolant computation"),
            &values[..],
            coset_idx.iter(),
            &fri_challenges[0..coset_size], 
        )?;

        for ((query, commitment), challenges) 
            in queries.into_iter().zip(commitments.iter()).zip(fri_challenges.chunks(coset_size).skip(1)) 
        {
            // adapt fri_helper for smaller domain
            fri_helper.next_domain(cs.namespace(|| "shrink domain to next layer"));

            // new natural_elem_index = (old_natural_element_index << collapsing_factor) % domain_size
            natural_index = &natural_index[collapsing_factor..fri_helper.get_log_domain_size()];
            let coset_idx = fri_helper.get_coset_idx_for_natural_index(natural_index);
            let offset = fri_helper.get_coset_offset_for_natural_index(natural_index);

            // oracle proof for current layer!
            let oracle_check = oracle.validate(
                cs.namespace(|| "Oracle proof"),
                fri_helper.get_log_domain_size(),
                &query.values, 
                coset_idx,
                commitment, 
                &query.proof, 
            )?;

            final_result = Boolean::and(cs.namespace(|| "and"), &final_result, &oracle_check)?;

            // round consistency check (rcc) : previous layer element interpolant has already been stored
            // compare it with current layer element (which is chosen from query values by offset)
            let cur_layer_element = fri_helper.choose_element_in_coset(
                cs.namespace(|| "choose element from coset by index"),
                &mut query.values[..],
                offset,
            )?; 
            let rcc_flag = AllocatedNum::equals(
                cs.namespace(|| "FRI round consistency check"), 
                &previous_layer_element, 
                &cur_layer_element,
            )?;
            final_result = Boolean::and(cs.namespace(|| "and"), &final_result, &rcc_flag)?;

            //recompute interpolant (using current layer for now) 
            //and store it for use on the next iteration (or for final check)
            previous_layer_element = fri_helper.coset_interpolation_value(
                cs.namespace(|| "coset interpolant computation"),
                &query.values[..],
                coset_idx.iter(),
                &fri_challenges, 
            )?;
        }

        // finally we compare the last interpolant with the value f(\omega), 
        // where f is built from coefficients

        assert!(final_coefficients.len() > 0);
        let val = if final_coefficients.len() == 1 {
            // if len is 1 there is no need to create additional omega with constraint overhea
            final_coefficients[0].clone()
        }
        else {

            fri_helper.next_domain(cs.namespace(|| "shrink domain to final layer"));
            natural_index = &natural_index[collapsing_factor..fri_helper.get_log_domain_size()];
            let omega = fri_helper.get_bottom_layer_omega(cs.namespace(|| "final layer generator"))?;
            let ev_p = AllocatedNum::pow(
                cs.namespace(|| "poly eval: evaluation point"), 
                omega, 
                natural_index.iter(),
            )?;

            let mut t = ev_p.clone();
            let mut running_sum : Num<E> = final_coefficients[0].clone().into();

            for c in final_coefficients.iter().skip(1) {

                let term = t.mul(cs.namespace(|| "next term"), c)?;
                t = t.mul(cs.namespace(|| "t^i"), &ev_p)?;
                running_sum.mut_add_number_with_coeff(&term, E::Fr::one());
            }

            running_sum.simplify(cs.namespace(|| "simplification of running sum"))?
        };

        let flag = AllocatedNum::equals(
            cs.namespace(|| "FRI final round consistency check"), 
            &previous_layer_element, 
            &val,
        )?;
        final_result = Boolean::and(cs.namespace(|| "and"), &final_result, &flag)?;

        Ok(final_result)
    }


    pub fn verify_proof<CS: ConstraintSystem<E>>(

        mut cs: CS,
        oracle_params: &I::Params,
        // data that is shared among all Fri query rounds
        upper_layer_combiner: &CombinerFunction<E>,
        upper_layer_commitments: &[Labeled<I::Commitment>],
        commitments: &[I::Commitment],
        final_coefficients: &[AllocatedNum<E>],
        fri_challenges: &[AllocatedNum<E>], 

        query_rounds_data: Vec<FriSingleQueryRoundData<E, I>>,
    ) -> Result<Boolean, SynthesisError> 
    {
        
        // construct global parameters
        let mut final_result = Boolean::Constant(true);
        let unpacked_fri_challenges : AllocatedNum<E> = Vec::with_capacity(capacity: usize)


    //     let mut two = F::one();
    //     two.double();

    //     let two_inv = two.inverse().ok_or(
    //         SynthesisError::DivisionByZero
    //     )?;

    //     let domain = Domain::<F>::new_for_size((params.initial_degree_plus_one.get() * params.lde_factor) as u64)?;

    //     let omega = domain.generator;
    //     let omega_inv = omega.inverse().ok_or(
    //         SynthesisError::DivisionByZero
    //     )?;

    //     let collapsing_factor = params.collapsing_factor;
    //     let coset_size = 1 << collapsing_factor;
    //     let initial_domain_size = domain.size as usize;
    //     let log_initial_domain_size = log2_floor(initial_domain_size) as usize;

    //     if natural_element_indexes.len() != params.R || proof.final_coefficients.len() > params.final_degree_plus_one {
    //         return Ok(false);
    //     }


        
    //     for ((round, natural_first_element_index), upper_layer_query) in 
    //         proof.queries.iter().zip(natural_element_indexes.into_iter()).zip(proof.upper_layer_queries.iter()) {
            
    //         let valid = FriIop::<F, O, C>::verify_single_proof_round::<Func>(
    //             &upper_layer_query,
    //             &upper_layer_commitments,
    //             &upper_layer_combiner,
    //             round,
    //             &proof.commitments,
    //             &proof.final_coefficients,
    //             natural_first_element_index,
    //             fri_challenges,
    //             num_steps as usize,
    //             initial_domain_size,
    //             log_initial_domain_size,
    //             collapsing_factor,
    //             coset_size,
    //             &oracle_params,
    //             &omega,
    //             &omega_inv,
    //             &two_inv,
    //         )?;

    //         if !valid {
    //             return Ok(false);
    //         }
    //     }

    //     return Ok(true);
    // }

        Ok(final_result)
    }
}

