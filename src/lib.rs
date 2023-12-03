//! This library implements Nova, a high-speed recursive SNARK.
#![deny(
  warnings,
  unused,
  future_incompatible,
  nonstandard_style,
  rust_2018_idioms,
  missing_docs
)]
#![allow(non_snake_case)]
// #![forbid(unsafe_code)] // Commented for development with `Abomonation`

// private modules
mod bellpepper;
mod circuit;
mod digest;
mod nifs;

// public modules
pub mod constants;
pub mod errors;
pub mod gadgets;
pub mod provider;
pub mod r1cs;
pub mod spartan;
pub mod traits;

pub mod supernova;

use once_cell::sync::OnceCell;
use provider::traits::DlogGroup;

use crate::digest::{DigestComputer, SimpleDigestible};
use crate::{
  bellpepper::{
    r1cs::{NovaShape, NovaWitness},
    shape_cs::ShapeCS,
    solver::SatisfyingAssignment,
  },
  r1cs::R1CSResult,
};
use abomonation::Abomonation;
use abomonation_derive::Abomonation;
use bellpepper_core::ConstraintSystem;
use circuit::{NovaAugmentedCircuit, NovaAugmentedCircuitInputs, NovaAugmentedCircuitParams};
use constants::{BN_LIMB_WIDTH, BN_N_LIMBS, NUM_FE_WITHOUT_IO_FOR_CRHF, NUM_HASH_BITS};
use core::marker::PhantomData;
use errors::NovaError;
use ff::{Field, PrimeField};
use gadgets::utils::scalar_as_base;
use nifs::NIFS;
use r1cs::{
  CommitmentKeyHint, R1CSInstance, R1CSShape, R1CSWitness, RelaxedR1CSInstance, RelaxedR1CSWitness,
};
use serde::{Deserialize, Serialize};
use traits::{
  circuit::StepCircuit,
  commitment::{CommitmentEngineTrait, CommitmentTrait},
  snark::RelaxedR1CSSNARKTrait,
  AbsorbInROTrait, Engine, ROConstants, ROConstantsCircuit, ROTrait,
};

/// A type that holds parameters for the primary and secondary circuits of Nova and SuperNova
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Abomonation)]
#[serde(bound = "")]
#[abomonation_bounds(where <E::Scalar as PrimeField>::Repr: Abomonation)]
pub struct CircuitShape<E: Engine> {
  F_arity: usize,
  r1cs_shape: R1CSShape<E>,
}

impl<E: Engine> SimpleDigestible for CircuitShape<E> {}

impl<E: Engine> CircuitShape<E> {
  /// Create a new `CircuitShape`
  pub fn new(r1cs_shape: R1CSShape<E>, F_arity: usize) -> Self {
    Self {
      F_arity,
      r1cs_shape,
    }
  }

  /// Return the [CircuitShape]' digest.
  pub fn digest(&self) -> E::Scalar {
    let dc: DigestComputer<'_, <E as Engine>::Scalar, CircuitShape<E>> = DigestComputer::new(self);
    dc.digest().expect("Failure in computing digest")
  }
}

/// A type that holds public parameters of Nova
#[derive(Clone, PartialEq, Serialize, Deserialize, Abomonation)]
#[serde(bound = "")]
#[abomonation_bounds(
where
  E1: Engine<Base = <E2 as Engine>::Scalar>,
  E2: Engine<Base = <E1 as Engine>::Scalar>,
  C1: StepCircuit<E1::Scalar>,
  C2: StepCircuit<E2::Scalar>,
  <E1::Scalar as PrimeField>::Repr: Abomonation,
  <E2::Scalar as PrimeField>::Repr: Abomonation,
)]
pub struct PublicParams<E1, E2, C1, C2>
where
  E1: Engine<Base = <E2 as Engine>::Scalar>,
  E2: Engine<Base = <E1 as Engine>::Scalar>,
  C1: StepCircuit<E1::Scalar>,
  C2: StepCircuit<E2::Scalar>,
{
  F_arity_primary: usize,
  F_arity_secondary: usize,
  ro_consts_primary: ROConstants<E1>,
  ro_consts_circuit_primary: ROConstantsCircuit<E2>,
  ck_primary: CommitmentKey<E1>,
  circuit_shape_primary: CircuitShape<E1>,
  ro_consts_secondary: ROConstants<E2>,
  ro_consts_circuit_secondary: ROConstantsCircuit<E1>,
  ck_secondary: CommitmentKey<E2>,
  circuit_shape_secondary: CircuitShape<E2>,
  augmented_circuit_params_primary: NovaAugmentedCircuitParams,
  augmented_circuit_params_secondary: NovaAugmentedCircuitParams,
  #[abomonation_skip]
  #[serde(skip, default = "OnceCell::new")]
  digest: OnceCell<E1::Scalar>,
  _p: PhantomData<(C1, C2)>,
}

impl<E1, E2, C1, C2> SimpleDigestible for PublicParams<E1, E2, C1, C2>
where
  E1: Engine<Base = <E2 as Engine>::Scalar>,
  E2: Engine<Base = <E1 as Engine>::Scalar>,
  C1: StepCircuit<E1::Scalar>,
  C2: StepCircuit<E2::Scalar>,
{
}

impl<E1, E2, C1, C2> PublicParams<E1, E2, C1, C2>
where
  E1: Engine<Base = <E2 as Engine>::Scalar>,
  E2: Engine<Base = <E1 as Engine>::Scalar>,
  C1: StepCircuit<E1::Scalar>,
  C2: StepCircuit<E2::Scalar>,
{
  /// Set up builder to create `PublicParams` for a pair of circuits `C1` and `C2`.
  ///
  /// # Note
  ///
  /// Public parameters set up a number of bases for the homomorphic commitment scheme of Nova.
  ///
  /// Some final compressing SNARKs, like variants of Spartan, use computation commitments that require
  /// larger sizes for these parameters. These SNARKs provide a hint for these values by
  /// implementing `RelaxedR1CSSNARKTrait::ck_floor()`, which can be passed to this function.
  ///
  /// If you're not using such a SNARK, pass `nova_snark::traits::snark::default_ck_hint()` instead.
  ///
  /// # Arguments
  ///
  /// * `c_primary`: The primary circuit of type `C1`.
  /// * `c_secondary`: The secondary circuit of type `C2`.
  /// * `ck_hint1`: A `CommitmentKeyHint` for `G1`, which is a function that provides a hint
  ///   for the number of generators required in the commitment scheme for the primary circuit.
  /// * `ck_hint2`: A `CommitmentKeyHint` for `G2`, similar to `ck_hint1`, but for the secondary circuit.
  ///
  /// # Example
  ///
  /// ```rust
  /// # use nova_snark::spartan::ppsnark::RelaxedR1CSSNARK;
  /// # use nova_snark::provider::ipa_pc::EvaluationEngine;
  /// # use nova_snark::provider::{PallasEngine, VestaEngine};
  /// # use nova_snark::traits::{circuit::TrivialCircuit, Engine, snark::RelaxedR1CSSNARKTrait};
  /// use nova_snark::PublicParams;
  ///
  /// type E1 = PallasEngine;
  /// type E2 = VestaEngine;
  /// type EE<E> = EvaluationEngine<E>;
  /// type SPrime<E> = RelaxedR1CSSNARK<E, EE<E>>;
  ///
  /// let circuit1 = TrivialCircuit::<<E1 as Engine>::Scalar>::default();
  /// let circuit2 = TrivialCircuit::<<E2 as Engine>::Scalar>::default();
  /// // Only relevant for a SNARK using computational commitments, pass &(|_| 0)
  /// // or &*nova_snark::traits::snark::default_ck_hint() otherwise.
  /// let ck_hint1 = &*SPrime::<E1>::ck_floor();
  /// let ck_hint2 = &*SPrime::<E2>::ck_floor();
  ///
  /// let pp = PublicParams::setup(&circuit1, &circuit2, ck_hint1, ck_hint2);
  /// ```
  pub fn setup(
    c_primary: &C1,
    c_secondary: &C2,
    ck_hint1: &CommitmentKeyHint<E1>,
    ck_hint2: &CommitmentKeyHint<E2>,
  ) -> Self {
    let augmented_circuit_params_primary =
      NovaAugmentedCircuitParams::new(BN_LIMB_WIDTH, BN_N_LIMBS, true);
    let augmented_circuit_params_secondary =
      NovaAugmentedCircuitParams::new(BN_LIMB_WIDTH, BN_N_LIMBS, false);

    let ro_consts_primary: ROConstants<E1> = ROConstants::<E1>::default();
    let ro_consts_secondary: ROConstants<E2> = ROConstants::<E2>::default();

    let F_arity_primary = c_primary.arity();
    let F_arity_secondary = c_secondary.arity();

    // ro_consts_circuit_primary are parameterized by E2 because the type alias uses E2::Base = E1::Scalar
    let ro_consts_circuit_primary: ROConstantsCircuit<E2> = ROConstantsCircuit::<E2>::default();
    let ro_consts_circuit_secondary: ROConstantsCircuit<E1> = ROConstantsCircuit::<E1>::default();

    // Initialize ck for the primary
    let circuit_primary: NovaAugmentedCircuit<'_, E2, C1> = NovaAugmentedCircuit::new(
      &augmented_circuit_params_primary,
      None,
      c_primary,
      ro_consts_circuit_primary.clone(),
    );
    let mut cs: ShapeCS<E1> = ShapeCS::new();
    let _ = circuit_primary.synthesize(&mut cs);
    let (r1cs_shape_primary, ck_primary) = cs.r1cs_shape_and_key(ck_hint1);
    let circuit_shape_primary = CircuitShape::new(r1cs_shape_primary, F_arity_primary);

    // Initialize ck for the secondary
    let circuit_secondary: NovaAugmentedCircuit<'_, E1, C2> = NovaAugmentedCircuit::new(
      &augmented_circuit_params_secondary,
      None,
      c_secondary,
      ro_consts_circuit_secondary.clone(),
    );
    let mut cs: ShapeCS<E2> = ShapeCS::new();
    let _ = circuit_secondary.synthesize(&mut cs);
    let (r1cs_shape_secondary, ck_secondary) = cs.r1cs_shape_and_key(ck_hint2);
    let circuit_shape_secondary = CircuitShape::new(r1cs_shape_secondary, F_arity_secondary);

    PublicParams {
      F_arity_primary,
      F_arity_secondary,
      ro_consts_primary,
      ro_consts_circuit_primary,
      ck_primary,
      circuit_shape_primary,
      ro_consts_secondary,
      ro_consts_circuit_secondary,
      ck_secondary,
      circuit_shape_secondary,
      augmented_circuit_params_primary,
      augmented_circuit_params_secondary,
      digest: OnceCell::new(),
      _p: Default::default(),
    }
  }

  /// Retrieve the digest of the public parameters.
  pub fn digest(&self) -> E1::Scalar {
    self
      .digest
      .get_or_try_init(|| DigestComputer::new(self).digest())
      .cloned()
      .expect("Failure in retrieving digest")
  }

  /// Returns the number of constraints in the primary and secondary circuits
  pub const fn num_constraints(&self) -> (usize, usize) {
    (
      self.circuit_shape_primary.r1cs_shape.num_cons,
      self.circuit_shape_secondary.r1cs_shape.num_cons,
    )
  }

  /// Returns the number of variables in the primary and secondary circuits
  pub const fn num_variables(&self) -> (usize, usize) {
    (
      self.circuit_shape_primary.r1cs_shape.num_vars,
      self.circuit_shape_secondary.r1cs_shape.num_vars,
    )
  }
}

/// A resource buffer for [`RecursiveSNARK`] for storing scratch values that are computed by `prove_step`,
/// which allows the reuse of memory allocations and avoids unnecessary new allocations in the critical section.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct ResourceBuffer<E: Engine> {
  l_w: Option<R1CSWitness<E>>,
  l_u: Option<R1CSInstance<E>>,

  ABC_Z_1: R1CSResult<E>,
  ABC_Z_2: R1CSResult<E>,

  /// buffer for `commit_T`
  T: Vec<E::Scalar>,

  #[serde(skip)]
  msm_context: <E::GE as DlogGroup>::MSMContext,
  #[serde(skip)]
  spmvm_context_A: <E::GE as DlogGroup>::SpMVMContext,
  #[serde(skip)]
  spmvm_context_B: <E::GE as DlogGroup>::SpMVMContext,
  #[serde(skip)]
  spmvm_context_C: <E::GE as DlogGroup>::SpMVMContext,
}

/// A SNARK that proves the correct execution of an incremental computation
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct RecursiveSNARK<E1, E2, C1, C2>
where
  E1: Engine<Base = <E2 as Engine>::Scalar>,
  E2: Engine<Base = <E1 as Engine>::Scalar>,
  C1: StepCircuit<E1::Scalar>,
  C2: StepCircuit<E2::Scalar>,
{
  z0_primary: Vec<E1::Scalar>,
  z0_secondary: Vec<E2::Scalar>,
  r_W_primary: RelaxedR1CSWitness<E1>,
  r_U_primary: RelaxedR1CSInstance<E1>,
  r_W_secondary: RelaxedR1CSWitness<E2>,
  r_U_secondary: RelaxedR1CSInstance<E2>,
  l_w_secondary: R1CSWitness<E2>,
  l_u_secondary: R1CSInstance<E2>,

  /// Buffer for memory needed by the primary fold-step
  buffer_primary: ResourceBuffer<E1>,
  /// Buffer for memory needed by the secondary fold-step
  buffer_secondary: ResourceBuffer<E2>,

  i: usize,
  zi_primary: Vec<E1::Scalar>,
  zi_secondary: Vec<E2::Scalar>,
  _p: PhantomData<(C1, C2)>,
}

impl<E1, E2, C1, C2> RecursiveSNARK<E1, E2, C1, C2>
where
  E1: Engine<Base = <E2 as Engine>::Scalar>,
  E2: Engine<Base = <E1 as Engine>::Scalar>,
  C1: StepCircuit<E1::Scalar>,
  C2: StepCircuit<E2::Scalar>,
{
  /// Create new instance of recursive SNARK
  pub fn new(
    pp: &PublicParams<E1, E2, C1, C2>,
    c_primary: &C1,
    c_secondary: &C2,
    z0_primary: &[E1::Scalar],
    z0_secondary: &[E2::Scalar],
  ) -> Result<Self, NovaError> {
    if z0_primary.len() != pp.F_arity_primary || z0_secondary.len() != pp.F_arity_secondary {
      return Err(NovaError::InvalidInitialInputLength);
    }

    let r1cs_primary = &pp.circuit_shape_primary.r1cs_shape;
    let r1cs_secondary = &pp.circuit_shape_secondary.r1cs_shape;

    tracing::info!("r1cs_primary statistics:");
    r1cs_primary.trace_statistics();

    tracing::info!("r1cs_secondary statistics:");
    r1cs_secondary.trace_statistics();

    let msm_context_primary = if E1::CE::has_preallocated_msm() {
      E1::CE::commit_init(&pp.ck_primary, r1cs_primary.num_cons)
    } else {
      <E1::GE as DlogGroup>::MSMContext::default()
    };

    let msm_context_secondary = if E2::CE::has_preallocated_msm() {
      E2::CE::commit_init(&pp.ck_secondary, r1cs_secondary.num_cons)
    } else {
      <E2::GE as DlogGroup>::MSMContext::default()
    };

    // base case for the primary
    let mut cs_primary = SatisfyingAssignment::<E1>::new();
    let inputs_primary: NovaAugmentedCircuitInputs<E2> = NovaAugmentedCircuitInputs::new(
      scalar_as_base::<E1>(pp.digest()),
      E1::Scalar::ZERO,
      z0_primary.to_vec(),
      None,
      None,
      None,
      None,
    );

    let circuit_primary: NovaAugmentedCircuit<'_, E2, C1> = NovaAugmentedCircuit::new(
      &pp.augmented_circuit_params_primary,
      Some(inputs_primary),
      c_primary,
      pp.ro_consts_circuit_primary.clone(),
    );
    let zi_primary = circuit_primary
      .synthesize(&mut cs_primary)
      .map_err(|_| NovaError::SynthesisError)
      .expect("Nova error synthesis");
    let (u_primary, w_primary) = cs_primary
      .r1cs_instance_and_witness_with(r1cs_primary, &msm_context_primary)
      .map_err(|_e| NovaError::UnSat)
      .expect("Nova error unsat");

    // base case for the secondary
    let mut cs_secondary = SatisfyingAssignment::<E2>::new();
    let inputs_secondary: NovaAugmentedCircuitInputs<E1> = NovaAugmentedCircuitInputs::new(
      pp.digest(),
      E2::Scalar::ZERO,
      z0_secondary.to_vec(),
      None,
      None,
      Some(u_primary.clone()),
      None,
    );
    let circuit_secondary: NovaAugmentedCircuit<'_, E1, C2> = NovaAugmentedCircuit::new(
      &pp.augmented_circuit_params_secondary,
      Some(inputs_secondary),
      c_secondary,
      pp.ro_consts_circuit_secondary.clone(),
    );
    let zi_secondary = circuit_secondary
      .synthesize(&mut cs_secondary)
      .map_err(|_| NovaError::SynthesisError)
      .expect("Nova error synthesis");
    let (u_secondary, w_secondary) = cs_secondary
      .r1cs_instance_and_witness_with(
        &pp.circuit_shape_secondary.r1cs_shape,
        &msm_context_secondary,
      )
      .map_err(|_e| NovaError::UnSat)
      .expect("Nova error unsat");

    // IVC proof for the primary circuit
    let l_w_primary = w_primary;
    let l_u_primary = u_primary;
    let r_W_primary = RelaxedR1CSWitness::from_r1cs_witness(r1cs_primary, l_w_primary);
    let r_U_primary = RelaxedR1CSInstance::from_r1cs_instance(
      &pp.ck_primary,
      &pp.circuit_shape_primary.r1cs_shape,
      l_u_primary,
    );

    // IVC proof for the secondary circuit
    let l_w_secondary = w_secondary;
    let l_u_secondary = u_secondary;
    let r_W_secondary = RelaxedR1CSWitness::<E2>::default(r1cs_secondary);
    let r_U_secondary = RelaxedR1CSInstance::<E2>::default(&pp.ck_secondary, r1cs_secondary);

    assert!(
      !(zi_primary.len() != pp.F_arity_primary || zi_secondary.len() != pp.F_arity_secondary),
      "Invalid step length"
    );

    let zi_primary = zi_primary
      .iter()
      .map(|v| v.get_value().ok_or(NovaError::SynthesisError))
      .collect::<Result<Vec<<E1 as Engine>::Scalar>, NovaError>>()
      .expect("Nova error synthesis");

    let zi_secondary = zi_secondary
      .iter()
      .map(|v| v.get_value().ok_or(NovaError::SynthesisError))
      .collect::<Result<Vec<<E2 as Engine>::Scalar>, NovaError>>()
      .expect("Nova error synthesis");

    let spmvm_context_A_primary = E1::GE::multiply_witness_into_init(&r1cs_primary.A);
    let spmvm_context_B_primary = E1::GE::multiply_witness_into_init(&r1cs_primary.B);
    let spmvm_context_C_primary = E1::GE::multiply_witness_into_init(&r1cs_primary.C);

    let mut buffer_primary = ResourceBuffer {
      l_w: None,
      l_u: None,
      ABC_Z_1: R1CSResult::default(r1cs_primary),
      ABC_Z_2: R1CSResult::default(r1cs_primary),
      T: r1cs::default_T(r1cs_primary),
      msm_context: msm_context_primary,
      spmvm_context_A: spmvm_context_A_primary,
      spmvm_context_B: spmvm_context_B_primary,
      spmvm_context_C: spmvm_context_C_primary,
    };

    r1cs_primary.multiply_witness_into(
      &r_W_primary.W,
      &r_U_primary.u,
      &r_U_primary.X,
      &mut buffer_primary.ABC_Z_1,
    )?;

    let spmvm_context_A_secondary = E2::GE::multiply_witness_into_init(&r1cs_secondary.A);
    let spmvm_context_B_secondary = E2::GE::multiply_witness_into_init(&r1cs_secondary.B);
    let spmvm_context_C_secondary = E2::GE::multiply_witness_into_init(&r1cs_secondary.C);

    let mut buffer_secondary = ResourceBuffer {
      l_w: None,
      l_u: None,
      ABC_Z_1: R1CSResult::default(r1cs_secondary),
      ABC_Z_2: R1CSResult::default(r1cs_secondary),
      T: r1cs::default_T(r1cs_secondary),
      msm_context: msm_context_secondary,
      spmvm_context_A: spmvm_context_A_secondary,
      spmvm_context_B: spmvm_context_B_secondary,
      spmvm_context_C: spmvm_context_C_secondary,
    };

    r1cs_secondary.multiply_witness_into(
      &r_W_secondary.W,
      &r_U_secondary.u,
      &r_U_secondary.X,
      &mut buffer_secondary.ABC_Z_1,
    )?;

    Ok(Self {
      z0_primary: z0_primary.to_vec(),
      z0_secondary: z0_secondary.to_vec(),
      r_W_primary,
      r_U_primary,
      r_W_secondary,
      r_U_secondary,
      l_w_secondary,
      l_u_secondary,

      buffer_primary,
      buffer_secondary,
      i: 0,
      zi_primary,
      zi_secondary,
      _p: Default::default(),
    })
  }

  /// Create a new `RecursiveSNARK` (or updates the provided `RecursiveSNARK`)
  /// by executing a step of the incremental computation
  #[tracing::instrument(skip_all, name = "nova::RecursiveSNARK::prove_step")]
  pub fn prove_step(
    &mut self,
    pp: &PublicParams<E1, E2, C1, C2>,
    c_primary: &C1,
    c_secondary: &C2,
  ) -> Result<(), NovaError> {
    // first step was already done in the constructor
    if self.i == 0 {
      self.i = 1;
      return Ok(());
    }

    // save the inputs before proceeding to the `i+1`th step
    let r_U_primary_i = self.r_U_primary.clone();
    let r_U_secondary_i = self.r_U_secondary.clone();
    let l_u_secondary_i = self.l_u_secondary.clone();

    // fold the secondary circuit's instance
    let nifs_secondary = NIFS::prove_mut(
      &pp.ck_secondary,
      &pp.ro_consts_secondary,
      &scalar_as_base::<E1>(pp.digest()),
      &pp.circuit_shape_secondary.r1cs_shape,
      &mut self.r_U_secondary,
      &mut self.r_W_secondary,
      &self.l_u_secondary,
      &self.l_w_secondary,
      &mut self.buffer_secondary,
    )
    .expect("Unable to fold secondary");

    let mut cs_primary = SatisfyingAssignment::<E1>::with_capacity(
      pp.circuit_shape_primary.r1cs_shape.num_io + 1,
      pp.circuit_shape_primary.r1cs_shape.num_vars,
    );
    let inputs_primary: NovaAugmentedCircuitInputs<E2> = NovaAugmentedCircuitInputs::new(
      scalar_as_base::<E1>(pp.digest()),
      E1::Scalar::from(self.i as u64),
      self.z0_primary.to_vec(),
      Some(self.zi_primary.clone()),
      Some(r_U_secondary_i),
      Some(l_u_secondary_i),
      Some(Commitment::<E2>::decompress(&nifs_secondary.comm_T)?),
    );

    let circuit_primary: NovaAugmentedCircuit<'_, E2, C1> = NovaAugmentedCircuit::new(
      &pp.augmented_circuit_params_primary,
      Some(inputs_primary),
      c_primary,
      pp.ro_consts_circuit_primary.clone(),
    );

    let zi_primary = circuit_primary
      .synthesize(&mut cs_primary)
      .map_err(|_| NovaError::SynthesisError)?;

    let (l_u_primary, l_w_primary) = cs_primary
      .r1cs_instance_and_witness_with(
        &pp.circuit_shape_primary.r1cs_shape,
        &self.buffer_primary.msm_context,
      )
      .map_err(|_e| NovaError::UnSat)
      .expect("Nova error unsat");

    // fold the primary circuit's instance
    let nifs_primary = NIFS::prove_mut(
      &pp.ck_primary,
      &pp.ro_consts_primary,
      &pp.digest(),
      &pp.circuit_shape_primary.r1cs_shape,
      &mut self.r_U_primary,
      &mut self.r_W_primary,
      &l_u_primary,
      &l_w_primary,
      &mut self.buffer_primary,
    )
    .expect("Unable to fold primary");

    let mut cs_secondary = SatisfyingAssignment::<E2>::with_capacity(
      pp.circuit_shape_secondary.r1cs_shape.num_io + 1,
      pp.circuit_shape_secondary.r1cs_shape.num_vars,
    );
    let inputs_secondary: NovaAugmentedCircuitInputs<E1> = NovaAugmentedCircuitInputs::new(
      pp.digest(),
      E2::Scalar::from(self.i as u64),
      self.z0_secondary.to_vec(),
      Some(self.zi_secondary.clone()),
      Some(r_U_primary_i),
      Some(l_u_primary),
      Some(Commitment::<E1>::decompress(&nifs_primary.comm_T)?),
    );

    let circuit_secondary: NovaAugmentedCircuit<'_, E1, C2> = NovaAugmentedCircuit::new(
      &pp.augmented_circuit_params_secondary,
      Some(inputs_secondary),
      c_secondary,
      pp.ro_consts_circuit_secondary.clone(),
    );
    let zi_secondary = circuit_secondary
      .synthesize(&mut cs_secondary)
      .map_err(|_| NovaError::SynthesisError)?;

    let (l_u_secondary, l_w_secondary) = cs_secondary
      .r1cs_instance_and_witness_with(
        &pp.circuit_shape_secondary.r1cs_shape,
        &self.buffer_secondary.msm_context,
      )
      .map_err(|_e| NovaError::UnSat)?;

    // update the running instances and witnesses
    self.zi_primary = zi_primary
      .iter()
      .map(|v| v.get_value().ok_or(NovaError::SynthesisError))
      .collect::<Result<Vec<<E1 as Engine>::Scalar>, NovaError>>()?;
    self.zi_secondary = zi_secondary
      .iter()
      .map(|v| v.get_value().ok_or(NovaError::SynthesisError))
      .collect::<Result<Vec<<E2 as Engine>::Scalar>, NovaError>>()?;

    self.l_u_secondary = l_u_secondary;
    self.l_w_secondary = l_w_secondary;

    self.i += 1;

    Ok(())
  }

  /// Verify the correctness of the `RecursiveSNARK`
  pub fn verify(
    &self,
    pp: &PublicParams<E1, E2, C1, C2>,
    num_steps: usize,
    z0_primary: &[E1::Scalar],
    z0_secondary: &[E2::Scalar],
  ) -> Result<(Vec<E1::Scalar>, Vec<E2::Scalar>), NovaError> {
    // number of steps cannot be zero
    let is_num_steps_zero = num_steps == 0;

    // check if the provided proof has executed num_steps
    let is_num_steps_not_match = self.i != num_steps;

    // check if the initial inputs match
    let is_inputs_not_match = self.z0_primary != z0_primary || self.z0_secondary != z0_secondary;

    // check if the (relaxed) R1CS instances have two public outputs
    let is_instance_has_two_outpus = self.l_u_secondary.X.len() != 2
      || self.r_U_primary.X.len() != 2
      || self.r_U_secondary.X.len() != 2;

    if is_num_steps_zero
      || is_num_steps_not_match
      || is_inputs_not_match
      || is_instance_has_two_outpus
    {
      return Err(NovaError::ProofVerifyError);
    }

    // check if the output hashes in R1CS instances point to the right running instances
    let (hash_primary, hash_secondary) = {
      let mut hasher = <E2 as Engine>::RO::new(
        pp.ro_consts_secondary.clone(),
        NUM_FE_WITHOUT_IO_FOR_CRHF + 2 * pp.F_arity_primary,
      );
      hasher.absorb(pp.digest());
      hasher.absorb(E1::Scalar::from(num_steps as u64));
      for e in z0_primary {
        hasher.absorb(*e);
      }
      for e in &self.zi_primary {
        hasher.absorb(*e);
      }
      self.r_U_secondary.absorb_in_ro(&mut hasher);

      let mut hasher2 = <E1 as Engine>::RO::new(
        pp.ro_consts_primary.clone(),
        NUM_FE_WITHOUT_IO_FOR_CRHF + 2 * pp.F_arity_secondary,
      );
      hasher2.absorb(scalar_as_base::<E1>(pp.digest()));
      hasher2.absorb(E2::Scalar::from(num_steps as u64));
      for e in z0_secondary {
        hasher2.absorb(*e);
      }
      for e in &self.zi_secondary {
        hasher2.absorb(*e);
      }
      self.r_U_primary.absorb_in_ro(&mut hasher2);

      (
        hasher.squeeze(NUM_HASH_BITS),
        hasher2.squeeze(NUM_HASH_BITS),
      )
    };

    if hash_primary != self.l_u_secondary.X[0]
      || hash_secondary != scalar_as_base::<E2>(self.l_u_secondary.X[1])
    {
      return Err(NovaError::ProofVerifyError);
    }

    // check the satisfiability of the provided instances
    let (res_r_primary, (res_r_secondary, res_l_secondary)) = rayon::join(
      || {
        pp.circuit_shape_primary.r1cs_shape.is_sat_relaxed(
          &pp.ck_primary,
          &self.r_U_primary,
          &self.r_W_primary,
        )
      },
      || {
        rayon::join(
          || {
            pp.circuit_shape_secondary.r1cs_shape.is_sat_relaxed(
              &pp.ck_secondary,
              &self.r_U_secondary,
              &self.r_W_secondary,
            )
          },
          || {
            pp.circuit_shape_secondary.r1cs_shape.is_sat(
              &pp.ck_secondary,
              &self.l_u_secondary,
              &self.l_w_secondary,
            )
          },
        )
      },
    );

    // check the returned res objects
    res_r_primary?;
    res_r_secondary?;
    res_l_secondary?;

    Ok((self.zi_primary.clone(), self.zi_secondary.clone()))
  }

  /// Writes the R1CS matrices and commitment key to
  /// `$HOME/.arecibo/*`
  pub fn write_abomonated(&self, pp: &PublicParams<E1, E2, C1, C2>) -> std::io::Result<()>
  where
    // this is due to the reliance on Abomonation
    <E1::Scalar as PrimeField>::Repr: Abomonation,
    <E2::Scalar as PrimeField>::Repr: Abomonation,
  {
    use std::fs::OpenOptions;
    use std::io::BufWriter;

    let arecibo = home::home_dir().unwrap().join(".arecibo");

    let r1cs_primary = OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .open(arecibo.join("r1cs_primary"))?;
    let mut writer = BufWriter::new(r1cs_primary);

    unsafe {
      abomonation::encode(
        &pp.circuit_shape_primary.r1cs_shape,
        &mut writer,
      )?
    };

    let r1cs_secondary = OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .open(arecibo.join("r1cs_secondary"))?;
    let mut writer = BufWriter::new(r1cs_secondary);

    unsafe {
      abomonation::encode(
        &pp.circuit_shape_secondary.r1cs_shape,
        &mut writer,
      )?
    };

    let witness_primary = OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .open(arecibo.join("witness_primary"))?;
    let mut writer = BufWriter::new(witness_primary);

    unsafe {
      abomonation::encode(
        std::mem::transmute::<&Vec<E1::Scalar>, &Vec<<E1::Scalar as PrimeField>::Repr>>(&self.r_W_primary.W),
        &mut writer,
      )?
    };

    let witness_secondary = OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .open(arecibo.join("witness_secondary"))?;
    let mut writer = BufWriter::new(witness_secondary);

    unsafe {
      abomonation::encode(
        std::mem::transmute::<&Vec<E2::Scalar>, &Vec<<E2::Scalar as PrimeField>::Repr>>(&self.r_W_secondary.W),
        &mut writer,
      )?
    };

    let ck_primary = OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .open(arecibo.join("ck_primary"))?;
    let mut writer = BufWriter::new(ck_primary);

    unsafe {
      abomonation::encode(
        &pp.ck_primary,
        &mut writer,
      )?
    };


    let ck_secondary = OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .open(arecibo.join("ck_secondary"))?;
    let mut writer = BufWriter::new(ck_secondary);

    unsafe {
      abomonation::encode(
        &pp.ck_secondary,
        &mut writer,
      )?
    };

    Ok(())
  }
}

/// A type that holds the prover key for `CompressedSNARK`
#[derive(Clone, Debug, Serialize, Deserialize, Abomonation)]
#[serde(bound = "")]
#[abomonation_omit_bounds]
pub struct ProverKey<E1, E2, C1, C2, S1, S2>
where
  E1: Engine<Base = <E2 as Engine>::Scalar>,
  E2: Engine<Base = <E1 as Engine>::Scalar>,
  C1: StepCircuit<E1::Scalar>,
  C2: StepCircuit<E2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<E1>,
  S2: RelaxedR1CSSNARKTrait<E2>,
{
  pk_primary: S1::ProverKey,
  pk_secondary: S2::ProverKey,
  _p: PhantomData<(C1, C2)>,
}

/// A type that holds the verifier key for `CompressedSNARK`
#[derive(Clone, Serialize, Deserialize, Abomonation)]
#[serde(bound = "")]
#[abomonation_bounds(
  where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    C1: StepCircuit<E1::Scalar>,
    C2: StepCircuit<E2::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
    <E1::Scalar as PrimeField>::Repr: Abomonation,
  )]
pub struct VerifierKey<E1, E2, C1, C2, S1, S2>
where
  E1: Engine<Base = <E2 as Engine>::Scalar>,
  E2: Engine<Base = <E1 as Engine>::Scalar>,
  C1: StepCircuit<E1::Scalar>,
  C2: StepCircuit<E2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<E1>,
  S2: RelaxedR1CSSNARKTrait<E2>,
{
  F_arity_primary: usize,
  F_arity_secondary: usize,
  ro_consts_primary: ROConstants<E1>,
  ro_consts_secondary: ROConstants<E2>,
  #[abomonate_with(<E1::Scalar as PrimeField>::Repr)]
  pp_digest: E1::Scalar,
  vk_primary: S1::VerifierKey,
  vk_secondary: S2::VerifierKey,
  _p: PhantomData<(C1, C2)>,
}

/// A SNARK that proves the knowledge of a valid `RecursiveSNARK`
#[derive(Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct CompressedSNARK<E1, E2, C1, C2, S1, S2>
where
  E1: Engine<Base = <E2 as Engine>::Scalar>,
  E2: Engine<Base = <E1 as Engine>::Scalar>,
  C1: StepCircuit<E1::Scalar>,
  C2: StepCircuit<E2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<E1>,
  S2: RelaxedR1CSSNARKTrait<E2>,
{
  r_U_primary: RelaxedR1CSInstance<E1>,
  r_W_snark_primary: S1,

  r_U_secondary: RelaxedR1CSInstance<E2>,
  l_u_secondary: R1CSInstance<E2>,
  nifs_secondary: NIFS<E2>,
  f_W_snark_secondary: S2,

  zn_primary: Vec<E1::Scalar>,
  zn_secondary: Vec<E2::Scalar>,

  _p: PhantomData<(C1, C2)>,
}

impl<E1, E2, C1, C2, S1, S2> CompressedSNARK<E1, E2, C1, C2, S1, S2>
where
  E1: Engine<Base = <E2 as Engine>::Scalar>,
  E2: Engine<Base = <E1 as Engine>::Scalar>,
  C1: StepCircuit<E1::Scalar>,
  C2: StepCircuit<E2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<E1>,
  S2: RelaxedR1CSSNARKTrait<E2>,
{
  /// Creates prover and verifier keys for `CompressedSNARK`
  pub fn setup(
    pp: &PublicParams<E1, E2, C1, C2>,
  ) -> Result<
    (
      ProverKey<E1, E2, C1, C2, S1, S2>,
      VerifierKey<E1, E2, C1, C2, S1, S2>,
    ),
    NovaError,
  > {
    let (pk_primary, vk_primary) = S1::setup(&pp.ck_primary, &pp.circuit_shape_primary.r1cs_shape)?;
    let (pk_secondary, vk_secondary) =
      S2::setup(&pp.ck_secondary, &pp.circuit_shape_secondary.r1cs_shape)?;

    let pk = ProverKey {
      pk_primary,
      pk_secondary,
      _p: Default::default(),
    };

    let vk = VerifierKey {
      F_arity_primary: pp.F_arity_primary,
      F_arity_secondary: pp.F_arity_secondary,
      ro_consts_primary: pp.ro_consts_primary.clone(),
      ro_consts_secondary: pp.ro_consts_secondary.clone(),
      pp_digest: pp.digest(),
      vk_primary,
      vk_secondary,
      _p: Default::default(),
    };

    Ok((pk, vk))
  }

  /// Create a new `CompressedSNARK`
  pub fn prove(
    pp: &PublicParams<E1, E2, C1, C2>,
    pk: &ProverKey<E1, E2, C1, C2, S1, S2>,
    recursive_snark: &RecursiveSNARK<E1, E2, C1, C2>,
  ) -> Result<Self, NovaError> {
    // fold the secondary circuit's instance with its running instance
    let (nifs_secondary, (f_U_secondary, f_W_secondary)) = NIFS::prove(
      &pp.ck_secondary,
      &pp.ro_consts_secondary,
      &scalar_as_base::<E1>(pp.digest()),
      &pp.circuit_shape_secondary.r1cs_shape,
      &recursive_snark.r_U_secondary,
      &recursive_snark.r_W_secondary,
      &recursive_snark.l_u_secondary,
      &recursive_snark.l_w_secondary,
    )?;

    // create SNARKs proving the knowledge of f_W_primary and f_W_secondary
    let (r_W_snark_primary, f_W_snark_secondary) = rayon::join(
      || {
        S1::prove(
          &pp.ck_primary,
          &pk.pk_primary,
          &pp.circuit_shape_primary.r1cs_shape,
          &recursive_snark.r_U_primary,
          &recursive_snark.r_W_primary,
        )
      },
      || {
        S2::prove(
          &pp.ck_secondary,
          &pk.pk_secondary,
          &pp.circuit_shape_secondary.r1cs_shape,
          &f_U_secondary,
          &f_W_secondary,
        )
      },
    );

    Ok(Self {
      r_U_primary: recursive_snark.r_U_primary.clone(),
      r_W_snark_primary: r_W_snark_primary?,

      r_U_secondary: recursive_snark.r_U_secondary.clone(),
      l_u_secondary: recursive_snark.l_u_secondary.clone(),
      nifs_secondary,
      f_W_snark_secondary: f_W_snark_secondary?,

      zn_primary: recursive_snark.zi_primary.clone(),
      zn_secondary: recursive_snark.zi_secondary.clone(),

      _p: Default::default(),
    })
  }

  /// Verify the correctness of the `CompressedSNARK`
  pub fn verify(
    &self,
    vk: &VerifierKey<E1, E2, C1, C2, S1, S2>,
    num_steps: usize,
    z0_primary: &[E1::Scalar],
    z0_secondary: &[E2::Scalar],
  ) -> Result<(Vec<E1::Scalar>, Vec<E2::Scalar>), NovaError> {
    // the number of steps cannot be zero
    if num_steps == 0 {
      return Err(NovaError::ProofVerifyError);
    }

    // check if the (relaxed) R1CS instances have two public outputs
    if self.l_u_secondary.X.len() != 2
      || self.r_U_primary.X.len() != 2
      || self.r_U_secondary.X.len() != 2
    {
      return Err(NovaError::ProofVerifyError);
    }

    // check if the output hashes in R1CS instances point to the right running instances
    let (hash_primary, hash_secondary) = {
      let mut hasher = <E2 as Engine>::RO::new(
        vk.ro_consts_secondary.clone(),
        NUM_FE_WITHOUT_IO_FOR_CRHF + 2 * vk.F_arity_primary,
      );
      hasher.absorb(vk.pp_digest);
      hasher.absorb(E1::Scalar::from(num_steps as u64));
      for e in z0_primary {
        hasher.absorb(*e);
      }
      for e in &self.zn_primary {
        hasher.absorb(*e);
      }
      self.r_U_secondary.absorb_in_ro(&mut hasher);

      let mut hasher2 = <E1 as Engine>::RO::new(
        vk.ro_consts_primary.clone(),
        NUM_FE_WITHOUT_IO_FOR_CRHF + 2 * vk.F_arity_secondary,
      );
      hasher2.absorb(scalar_as_base::<E1>(vk.pp_digest));
      hasher2.absorb(E2::Scalar::from(num_steps as u64));
      for e in z0_secondary {
        hasher2.absorb(*e);
      }
      for e in &self.zn_secondary {
        hasher2.absorb(*e);
      }
      self.r_U_primary.absorb_in_ro(&mut hasher2);

      (
        hasher.squeeze(NUM_HASH_BITS),
        hasher2.squeeze(NUM_HASH_BITS),
      )
    };

    if hash_primary != self.l_u_secondary.X[0]
      || hash_secondary != scalar_as_base::<E2>(self.l_u_secondary.X[1])
    {
      return Err(NovaError::ProofVerifyError);
    }

    // fold the secondary's running instance with the last instance to get a folded instance
    let f_U_secondary = self.nifs_secondary.verify(
      &vk.ro_consts_secondary,
      &scalar_as_base::<E1>(vk.pp_digest),
      &self.r_U_secondary,
      &self.l_u_secondary,
    )?;

    // check the satisfiability of the folded instances using
    // SNARKs proving the knowledge of their satisfying witnesses
    let (res_primary, res_secondary) = rayon::join(
      || {
        self
          .r_W_snark_primary
          .verify(&vk.vk_primary, &self.r_U_primary)
      },
      || {
        self
          .f_W_snark_secondary
          .verify(&vk.vk_secondary, &f_U_secondary)
      },
    );

    res_primary?;
    res_secondary?;

    Ok((self.zn_primary.clone(), self.zn_secondary.clone()))
  }
}

/// Compute the circuit digest of a [StepCircuit].
///
/// Note for callers: This function should be called with its performance characteristics in mind.
/// It will synthesize and digest the full `circuit` given.
pub fn circuit_digest<
  E1: Engine<Base = <E2 as Engine>::Scalar>,
  E2: Engine<Base = <E1 as Engine>::Scalar>,
  C: StepCircuit<E1::Scalar>,
>(
  circuit: &C,
) -> E1::Scalar {
  let augmented_circuit_params = NovaAugmentedCircuitParams::new(BN_LIMB_WIDTH, BN_N_LIMBS, true);

  // ro_consts_circuit are parameterized by G2 because the type alias uses G2::Base = G1::Scalar
  let ro_consts_circuit: ROConstantsCircuit<E2> = ROConstantsCircuit::<E2>::default();

  // Initialize ck for the primary
  let augmented_circuit: NovaAugmentedCircuit<'_, E2, C> =
    NovaAugmentedCircuit::new(&augmented_circuit_params, None, circuit, ro_consts_circuit);
  let mut cs: ShapeCS<E1> = ShapeCS::new();
  let _ = augmented_circuit.synthesize(&mut cs);
  cs.r1cs_shape().digest()
}

type CommitmentKey<E> = <<E as Engine>::CE as CommitmentEngineTrait<E>>::CommitmentKey;
type Commitment<E> = <<E as Engine>::CE as CommitmentEngineTrait<E>>::Commitment;
type CompressedCommitment<E> = <<<E as Engine>::CE as CommitmentEngineTrait<E>>::Commitment as CommitmentTrait<E>>::CompressedCommitment;
type CE<E> = <E as Engine>::CE;

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    provider::{
      Bn256Engine, GrumpkinEngine, PallasEngine, Secp256k1Engine, Secq256k1Engine, VestaEngine,
    },
    traits::{evaluation::EvaluationEngineTrait, snark::default_ck_hint},
  };
  use ::bellpepper_core::{num::AllocatedNum, ConstraintSystem, SynthesisError};
  use core::{fmt::Write, marker::PhantomData};
  use ff::PrimeField;
  use traits::circuit::TrivialCircuit;

  type EE<E> = provider::ipa_pc::EvaluationEngine<E>;
  type S<E, EE> = spartan::snark::RelaxedR1CSSNARK<E, EE>;
  type SPrime<E, EE> = spartan::ppsnark::RelaxedR1CSSNARK<E, EE>;

  #[derive(Clone, Debug, Default)]
  struct CubicCircuit<F: PrimeField> {
    _p: PhantomData<F>,
  }

  impl<F: PrimeField> StepCircuit<F> for CubicCircuit<F> {
    fn arity(&self) -> usize {
      1
    }

    fn synthesize<CS: ConstraintSystem<F>>(
      &self,
      cs: &mut CS,
      z: &[AllocatedNum<F>],
    ) -> Result<Vec<AllocatedNum<F>>, SynthesisError> {
      // Consider a cubic equation: `x^3 + x + 5 = y`, where `x` and `y` are respectively the input and output.
      let x = &z[0];
      let x_sq = x.square(cs.namespace(|| "x_sq"))?;
      let x_cu = x_sq.mul(cs.namespace(|| "x_cu"), x)?;
      let y = AllocatedNum::alloc(cs.namespace(|| "y"), || {
        Ok(x_cu.get_value().unwrap() + x.get_value().unwrap() + F::from(5u64))
      })?;

      cs.enforce(
        || "y = x^3 + x + 5",
        |lc| {
          lc + x_cu.get_variable()
            + x.get_variable()
            + CS::one()
            + CS::one()
            + CS::one()
            + CS::one()
            + CS::one()
        },
        |lc| lc + CS::one(),
        |lc| lc + y.get_variable(),
      );

      Ok(vec![y])
    }
  }

  impl<F: PrimeField> CubicCircuit<F> {
    fn output(&self, z: &[F]) -> Vec<F> {
      vec![z[0] * z[0] * z[0] + z[0] + F::from(5u64)]
    }
  }

  fn test_pp_digest_with<E1, E2, T1, T2, EE1, EE2>(circuit1: &T1, circuit2: &T2, expected: &str)
  where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    T1: StepCircuit<E1::Scalar>,
    T2: StepCircuit<E2::Scalar>,
    EE1: EvaluationEngineTrait<E1>,
    EE2: EvaluationEngineTrait<E2>,
    // this is due to the reliance on Abomonation
    <E1::Scalar as PrimeField>::Repr: Abomonation,
    <E2::Scalar as PrimeField>::Repr: Abomonation,
  {
    // this tests public parameters with a size specifically intended for a spark-compressed SNARK
    let ck_hint1 = &*SPrime::<E1, EE1>::ck_floor();
    let ck_hint2 = &*SPrime::<E2, EE2>::ck_floor();
    let pp = PublicParams::<E1, E2, T1, T2>::setup(circuit1, circuit2, ck_hint1, ck_hint2);

    let digest_str = pp
      .digest()
      .to_repr()
      .as_ref()
      .iter()
      .fold(String::new(), |mut output, b| {
        let _ = write!(output, "{b:02x}");
        output
      });
    assert_eq!(digest_str, expected);
  }

  #[test]
  fn test_pp_digest() {
    let trivial_circuit1 = TrivialCircuit::<<PallasEngine as Engine>::Scalar>::default();
    let trivial_circuit2 = TrivialCircuit::<<VestaEngine as Engine>::Scalar>::default();
    let cubic_circuit1 = CubicCircuit::<<PallasEngine as Engine>::Scalar>::default();

    test_pp_digest_with::<PallasEngine, VestaEngine, _, _, EE<_>, EE<_>>(
      &trivial_circuit1,
      &trivial_circuit2,
      "f4a04841515b4721519e2671b7ee11e58e2d4a30bb183ded963b71ad2ec80d00",
    );

    test_pp_digest_with::<PallasEngine, VestaEngine, _, _, EE<_>, EE<_>>(
      &cubic_circuit1,
      &trivial_circuit2,
      "dc1b7c40ab50c5c6877ad027769452870cc28f1d13f140de7ca3a00138c58502",
    );

    let trivial_circuit1_grumpkin = TrivialCircuit::<<Bn256Engine as Engine>::Scalar>::default();
    let trivial_circuit2_grumpkin = TrivialCircuit::<<GrumpkinEngine as Engine>::Scalar>::default();
    let cubic_circuit1_grumpkin = CubicCircuit::<<Bn256Engine as Engine>::Scalar>::default();

    // These tests should not need be different on the "asm" feature for bn256.
    // See https://github.com/privacy-scaling-explorations/halo2curves/issues/100 for why they are - closing the issue there
    // should eliminate the discrepancy here.
    #[cfg(feature = "asm")]
    test_pp_digest_with::<Bn256Engine, GrumpkinEngine, _, _, EE<_>, EE<_>>(
      &trivial_circuit1_grumpkin,
      &trivial_circuit2_grumpkin,
      "df834cb2bb401251473de2f3bbe6f6c28f1c6848f74dd8281faef91b73e7f400",
    );
    #[cfg(feature = "asm")]
    test_pp_digest_with::<Bn256Engine, GrumpkinEngine, _, _, EE<_>, EE<_>>(
      &cubic_circuit1_grumpkin,
      &trivial_circuit2_grumpkin,
      "97c009974d5b12b318045d623fff145006fab5f4234cb50e867d66377e191300",
    );
    #[cfg(not(feature = "asm"))]
    test_pp_digest_with::<Bn256Engine, GrumpkinEngine, _, _, EE<_>, EE<_>>(
      &trivial_circuit1_grumpkin,
      &trivial_circuit2_grumpkin,
      "c565748bea3336f07c8ff997c542ed62385ff5662f29402c4f9747153f699e01",
    );
    #[cfg(not(feature = "asm"))]
    test_pp_digest_with::<Bn256Engine, GrumpkinEngine, _, _, EE<_>, EE<_>>(
      &cubic_circuit1_grumpkin,
      &trivial_circuit2_grumpkin,
      "5aec6defcb0f6b2bb14aec70362419388916d7a5bc528c0b3fabb197ae57cb03",
    );

    let trivial_circuit1_secp = TrivialCircuit::<<Secp256k1Engine as Engine>::Scalar>::default();
    let trivial_circuit2_secp = TrivialCircuit::<<Secq256k1Engine as Engine>::Scalar>::default();
    let cubic_circuit1_secp = CubicCircuit::<<Secp256k1Engine as Engine>::Scalar>::default();

    test_pp_digest_with::<Secp256k1Engine, Secq256k1Engine, _, _, EE<_>, EE<_>>(
      &trivial_circuit1_secp,
      &trivial_circuit2_secp,
      "66c6d3618bb824bcb9253b7731247b89432853bf2014ffae45a8f6b00befe303",
    );
    test_pp_digest_with::<Secp256k1Engine, Secq256k1Engine, _, _, EE<_>, EE<_>>(
      &cubic_circuit1_secp,
      &trivial_circuit2_secp,
      "cc22c270460e11d190235fbd691bdeec51e8200219e5e65112e48bb80b610803",
    );
  }

  fn test_ivc_trivial_with<E1, E2>()
  where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
  {
    let test_circuit1 = TrivialCircuit::<<E1 as Engine>::Scalar>::default();
    let test_circuit2 = TrivialCircuit::<<E2 as Engine>::Scalar>::default();

    // produce public parameters
    let pp = PublicParams::<
      E1,
      E2,
      TrivialCircuit<<E1 as Engine>::Scalar>,
      TrivialCircuit<<E2 as Engine>::Scalar>,
    >::setup(
      &test_circuit1,
      &test_circuit2,
      &*default_ck_hint(),
      &*default_ck_hint(),
    );
    let num_steps = 1;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::new(
      &pp,
      &test_circuit1,
      &test_circuit2,
      &[<E1 as Engine>::Scalar::ZERO],
      &[<E2 as Engine>::Scalar::ZERO],
    )
    .unwrap();

    let res = recursive_snark.prove_step(&pp, &test_circuit1, &test_circuit2);

    assert!(res.is_ok());

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<E1 as Engine>::Scalar::ZERO],
      &[<E2 as Engine>::Scalar::ZERO],
    );
    assert!(res.is_ok());
  }

  #[test]
  fn test_ivc_trivial() {
    test_ivc_trivial_with::<PallasEngine, VestaEngine>();
    test_ivc_trivial_with::<Bn256Engine, GrumpkinEngine>();
    test_ivc_trivial_with::<Secp256k1Engine, Secq256k1Engine>();
  }

  fn test_ivc_nontrivial_with<E1, E2>()
  where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
  {
    let circuit_primary = TrivialCircuit::default();
    let circuit_secondary = CubicCircuit::default();

    // produce public parameters
    let pp = PublicParams::<
      E1,
      E2,
      TrivialCircuit<<E1 as Engine>::Scalar>,
      CubicCircuit<<E2 as Engine>::Scalar>,
    >::setup(
      &circuit_primary,
      &circuit_secondary,
      &*default_ck_hint(),
      &*default_ck_hint(),
    );

    let num_steps = 3;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::<
      E1,
      E2,
      TrivialCircuit<<E1 as Engine>::Scalar>,
      CubicCircuit<<E2 as Engine>::Scalar>,
    >::new(
      &pp,
      &circuit_primary,
      &circuit_secondary,
      &[<E1 as Engine>::Scalar::ONE],
      &[<E2 as Engine>::Scalar::ZERO],
    )
    .unwrap();

    for i in 0..num_steps {
      let res = recursive_snark.prove_step(&pp, &circuit_primary, &circuit_secondary);
      assert!(res.is_ok());

      // verify the recursive snark at each step of recursion
      let res = recursive_snark.verify(
        &pp,
        i + 1,
        &[<E1 as Engine>::Scalar::ONE],
        &[<E2 as Engine>::Scalar::ZERO],
      );
      assert!(res.is_ok());
    }

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<E1 as Engine>::Scalar::ONE],
      &[<E2 as Engine>::Scalar::ZERO],
    );
    assert!(res.is_ok());

    let (zn_primary, zn_secondary) = res.unwrap();

    // sanity: check the claimed output with a direct computation of the same
    assert_eq!(zn_primary, vec![<E1 as Engine>::Scalar::ONE]);
    let mut zn_secondary_direct = vec![<E2 as Engine>::Scalar::ZERO];
    for _i in 0..num_steps {
      zn_secondary_direct = circuit_secondary.clone().output(&zn_secondary_direct);
    }
    assert_eq!(zn_secondary, zn_secondary_direct);
    assert_eq!(zn_secondary, vec![<E2 as Engine>::Scalar::from(2460515u64)]);
  }

  #[test]
  fn test_ivc_nontrivial() {
    test_ivc_nontrivial_with::<PallasEngine, VestaEngine>();
    test_ivc_nontrivial_with::<Bn256Engine, GrumpkinEngine>();
    test_ivc_nontrivial_with::<Secp256k1Engine, Secq256k1Engine>();
  }

  fn test_ivc_nontrivial_with_compression_with<E1, E2, EE1, EE2>()
  where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    EE1: EvaluationEngineTrait<E1>,
    EE2: EvaluationEngineTrait<E2>,
    // this is due to the reliance on Abomonation
    <E1::Scalar as PrimeField>::Repr: Abomonation,
    <E2::Scalar as PrimeField>::Repr: Abomonation,
  {
    let circuit_primary = TrivialCircuit::default();
    let circuit_secondary = CubicCircuit::default();

    // produce public parameters
    let pp = PublicParams::<
      E1,
      E2,
      TrivialCircuit<<E1 as Engine>::Scalar>,
      CubicCircuit<<E2 as Engine>::Scalar>,
    >::setup(
      &circuit_primary,
      &circuit_secondary,
      &*default_ck_hint(),
      &*default_ck_hint(),
    );

    let num_steps = 3;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::<
      E1,
      E2,
      TrivialCircuit<<E1 as Engine>::Scalar>,
      CubicCircuit<<E2 as Engine>::Scalar>,
    >::new(
      &pp,
      &circuit_primary,
      &circuit_secondary,
      &[<E1 as Engine>::Scalar::ONE],
      &[<E2 as Engine>::Scalar::ZERO],
    )
    .unwrap();

    for _i in 0..num_steps {
      let res = recursive_snark.prove_step(&pp, &circuit_primary, &circuit_secondary);
      assert!(res.is_ok());
    }

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<E1 as Engine>::Scalar::ONE],
      &[<E2 as Engine>::Scalar::ZERO],
    );
    assert!(res.is_ok());

    let (zn_primary, zn_secondary) = res.unwrap();

    // sanity: check the claimed output with a direct computation of the same
    assert_eq!(zn_primary, vec![<E1 as Engine>::Scalar::ONE]);
    let mut zn_secondary_direct = vec![<E2 as Engine>::Scalar::ZERO];
    for _i in 0..num_steps {
      zn_secondary_direct = circuit_secondary.clone().output(&zn_secondary_direct);
    }
    assert_eq!(zn_secondary, zn_secondary_direct);
    assert_eq!(zn_secondary, vec![<E2 as Engine>::Scalar::from(2460515u64)]);

    // produce the prover and verifier keys for compressed snark
    let (pk, vk) = CompressedSNARK::<_, _, _, _, S<E1, EE1>, S<E2, EE2>>::setup(&pp).unwrap();

    // produce a compressed SNARK
    let res =
      CompressedSNARK::<_, _, _, _, S<E1, EE1>, S<E2, EE2>>::prove(&pp, &pk, &recursive_snark);
    assert!(res.is_ok());
    let compressed_snark = res.unwrap();

    // verify the compressed SNARK
    let res = compressed_snark.verify(
      &vk,
      num_steps,
      &[<E1 as Engine>::Scalar::ONE],
      &[<E2 as Engine>::Scalar::ZERO],
    );
    assert!(res.is_ok());
  }

  #[test]
  fn test_ivc_nontrivial_with_compression() {
    test_ivc_nontrivial_with_compression_with::<PallasEngine, VestaEngine, EE<_>, EE<_>>();
    test_ivc_nontrivial_with_compression_with::<Bn256Engine, GrumpkinEngine, EE<_>, EE<_>>();
    test_ivc_nontrivial_with_compression_with::<Secp256k1Engine, Secq256k1Engine, EE<_>, EE<_>>();
  }

  fn test_ivc_nontrivial_with_spark_compression_with<E1, E2, EE1, EE2>()
  where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    EE1: EvaluationEngineTrait<E1>,
    EE2: EvaluationEngineTrait<E2>,
    // this is due to the reliance on Abomonation
    <E1::Scalar as PrimeField>::Repr: Abomonation,
    <E2::Scalar as PrimeField>::Repr: Abomonation,
  {
    let circuit_primary = TrivialCircuit::default();
    let circuit_secondary = CubicCircuit::default();

    // produce public parameters, which we'll use with a spark-compressed SNARK
    let pp = PublicParams::<
      E1,
      E2,
      TrivialCircuit<<E1 as Engine>::Scalar>,
      CubicCircuit<<E2 as Engine>::Scalar>,
    >::setup(
      &circuit_primary,
      &circuit_secondary,
      &*SPrime::<E1, EE1>::ck_floor(),
      &*SPrime::<E2, EE2>::ck_floor(),
    );

    let num_steps = 3;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::<
      E1,
      E2,
      TrivialCircuit<<E1 as Engine>::Scalar>,
      CubicCircuit<<E2 as Engine>::Scalar>,
    >::new(
      &pp,
      &circuit_primary,
      &circuit_secondary,
      &[<E1 as Engine>::Scalar::ONE],
      &[<E2 as Engine>::Scalar::ZERO],
    )
    .unwrap();

    for _i in 0..num_steps {
      let res = recursive_snark.prove_step(&pp, &circuit_primary, &circuit_secondary);
      assert!(res.is_ok());
    }

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<E1 as Engine>::Scalar::ONE],
      &[<E2 as Engine>::Scalar::ZERO],
    );
    assert!(res.is_ok());

    let (zn_primary, zn_secondary) = res.unwrap();

    // sanity: check the claimed output with a direct computation of the same
    assert_eq!(zn_primary, vec![<E1 as Engine>::Scalar::ONE]);
    let mut zn_secondary_direct = vec![<E2 as Engine>::Scalar::ZERO];
    for _i in 0..num_steps {
      zn_secondary_direct = CubicCircuit::default().output(&zn_secondary_direct);
    }
    assert_eq!(zn_secondary, zn_secondary_direct);
    assert_eq!(zn_secondary, vec![<E2 as Engine>::Scalar::from(2460515u64)]);

    // run the compressed snark with Spark compiler
    // produce the prover and verifier keys for compressed snark
    let (pk, vk) =
      CompressedSNARK::<_, _, _, _, SPrime<E1, EE1>, SPrime<E2, EE2>>::setup(&pp).unwrap();

    // produce a compressed SNARK
    let res = CompressedSNARK::<_, _, _, _, SPrime<E1, EE1>, SPrime<E2, EE2>>::prove(
      &pp,
      &pk,
      &recursive_snark,
    );
    assert!(res.is_ok());
    let compressed_snark = res.unwrap();

    // verify the compressed SNARK
    let res = compressed_snark.verify(
      &vk,
      num_steps,
      &[<E1 as Engine>::Scalar::ONE],
      &[<E2 as Engine>::Scalar::ZERO],
    );
    assert!(res.is_ok());
  }

  #[test]
  fn test_ivc_nontrivial_with_spark_compression() {
    test_ivc_nontrivial_with_spark_compression_with::<PallasEngine, VestaEngine, EE<_>, EE<_>>();
    test_ivc_nontrivial_with_spark_compression_with::<Bn256Engine, GrumpkinEngine, EE<_>, EE<_>>();
    test_ivc_nontrivial_with_spark_compression_with::<Secp256k1Engine, Secq256k1Engine, EE<_>, EE<_>>(
    );
  }

  fn test_ivc_nondet_with_compression_with<E1, E2, EE1, EE2>()
  where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    EE1: EvaluationEngineTrait<E1>,
    EE2: EvaluationEngineTrait<E2>,
    // this is due to the reliance on Abomonation
    <E1::Scalar as PrimeField>::Repr: Abomonation,
    <E2::Scalar as PrimeField>::Repr: Abomonation,
  {
    // y is a non-deterministic advice representing the fifth root of the input at a step.
    #[derive(Clone, Debug)]
    struct FifthRootCheckingCircuit<F: PrimeField> {
      y: F,
    }

    impl<F: PrimeField> FifthRootCheckingCircuit<F> {
      fn new(num_steps: usize) -> (Vec<F>, Vec<Self>) {
        let mut powers = Vec::new();
        let rng = &mut rand::rngs::OsRng;
        let mut seed = F::random(rng);
        for _i in 0..num_steps + 1 {
          seed *= seed.clone().square().square();

          powers.push(Self { y: seed });
        }

        // reverse the powers to get roots
        let roots = powers.into_iter().rev().collect::<Vec<Self>>();
        (vec![roots[0].y], roots[1..].to_vec())
      }
    }

    impl<F> StepCircuit<F> for FifthRootCheckingCircuit<F>
    where
      F: PrimeField,
    {
      fn arity(&self) -> usize {
        1
      }

      fn synthesize<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        z: &[AllocatedNum<F>],
      ) -> Result<Vec<AllocatedNum<F>>, SynthesisError> {
        let x = &z[0];

        // we allocate a variable and set it to the provided non-deterministic advice.
        let y = AllocatedNum::alloc_infallible(cs.namespace(|| "y"), || self.y);

        // We now check if y = x^{1/5} by checking if y^5 = x
        let y_sq = y.square(cs.namespace(|| "y_sq"))?;
        let y_quad = y_sq.square(cs.namespace(|| "y_quad"))?;
        let y_pow_5 = y_quad.mul(cs.namespace(|| "y_fifth"), &y)?;

        cs.enforce(
          || "y^5 = x",
          |lc| lc + y_pow_5.get_variable(),
          |lc| lc + CS::one(),
          |lc| lc + x.get_variable(),
        );

        Ok(vec![y])
      }
    }

    let circuit_primary = FifthRootCheckingCircuit {
      y: <E1 as Engine>::Scalar::ZERO,
    };

    let circuit_secondary = TrivialCircuit::default();

    // produce public parameters
    let pp = PublicParams::<
      E1,
      E2,
      FifthRootCheckingCircuit<<E1 as Engine>::Scalar>,
      TrivialCircuit<<E2 as Engine>::Scalar>,
    >::setup(
      &circuit_primary,
      &circuit_secondary,
      &*default_ck_hint(),
      &*default_ck_hint(),
    );

    let num_steps = 3;

    // produce non-deterministic advice
    let (z0_primary, roots) = FifthRootCheckingCircuit::new(num_steps);
    let z0_secondary = vec![<E2 as Engine>::Scalar::ZERO];

    // produce a recursive SNARK
    let mut recursive_snark: RecursiveSNARK<
      E1,
      E2,
      FifthRootCheckingCircuit<<E1 as Engine>::Scalar>,
      TrivialCircuit<<E2 as Engine>::Scalar>,
    > = RecursiveSNARK::<
      E1,
      E2,
      FifthRootCheckingCircuit<<E1 as Engine>::Scalar>,
      TrivialCircuit<<E2 as Engine>::Scalar>,
    >::new(
      &pp,
      &roots[0],
      &circuit_secondary,
      &z0_primary,
      &z0_secondary,
    )
    .unwrap();

    for circuit_primary in roots.iter().take(num_steps) {
      let res = recursive_snark.prove_step(&pp, circuit_primary, &circuit_secondary);
      assert!(res.is_ok());
    }

    // verify the recursive SNARK
    let res = recursive_snark.verify(&pp, num_steps, &z0_primary, &z0_secondary);
    assert!(res.is_ok());

    // produce the prover and verifier keys for compressed snark
    let (pk, vk) = CompressedSNARK::<_, _, _, _, S<E1, EE1>, S<E2, EE2>>::setup(&pp).unwrap();

    // produce a compressed SNARK
    let res =
      CompressedSNARK::<_, _, _, _, S<E1, EE1>, S<E2, EE2>>::prove(&pp, &pk, &recursive_snark);
    assert!(res.is_ok());
    let compressed_snark = res.unwrap();

    // verify the compressed SNARK
    let res = compressed_snark.verify(&vk, num_steps, &z0_primary, &z0_secondary);
    assert!(res.is_ok());
  }

  #[test]
  fn test_ivc_nondet_with_compression() {
    test_ivc_nondet_with_compression_with::<PallasEngine, VestaEngine, EE<_>, EE<_>>();
    test_ivc_nondet_with_compression_with::<Bn256Engine, GrumpkinEngine, EE<_>, EE<_>>();
    test_ivc_nondet_with_compression_with::<Secp256k1Engine, Secq256k1Engine, EE<_>, EE<_>>();
  }

  fn test_ivc_base_with<E1, E2>()
  where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
  {
    let test_circuit1 = TrivialCircuit::<<E1 as Engine>::Scalar>::default();
    let test_circuit2 = CubicCircuit::<<E2 as Engine>::Scalar>::default();

    // produce public parameters
    let pp = PublicParams::<
      E1,
      E2,
      TrivialCircuit<<E1 as Engine>::Scalar>,
      CubicCircuit<<E2 as Engine>::Scalar>,
    >::setup(
      &test_circuit1,
      &test_circuit2,
      &*default_ck_hint(),
      &*default_ck_hint(),
    );

    let num_steps = 1;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::<
      E1,
      E2,
      TrivialCircuit<<E1 as Engine>::Scalar>,
      CubicCircuit<<E2 as Engine>::Scalar>,
    >::new(
      &pp,
      &test_circuit1,
      &test_circuit2,
      &[<E1 as Engine>::Scalar::ONE],
      &[<E2 as Engine>::Scalar::ZERO],
    )
    .unwrap();

    // produce a recursive SNARK
    let res = recursive_snark.prove_step(&pp, &test_circuit1, &test_circuit2);

    assert!(res.is_ok());

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<E1 as Engine>::Scalar::ONE],
      &[<E2 as Engine>::Scalar::ZERO],
    );
    assert!(res.is_ok());

    let (zn_primary, zn_secondary) = res.unwrap();

    assert_eq!(zn_primary, vec![<E1 as Engine>::Scalar::ONE]);
    assert_eq!(zn_secondary, vec![<E2 as Engine>::Scalar::from(5u64)]);
  }

  #[test]
  fn test_ivc_base() {
    test_ivc_base_with::<PallasEngine, VestaEngine>();
    test_ivc_base_with::<Bn256Engine, GrumpkinEngine>();
    test_ivc_base_with::<Secp256k1Engine, Secq256k1Engine>();
  }
}
