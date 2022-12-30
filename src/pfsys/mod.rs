/// Aggregation circuit
#[cfg(feature = "evm")]
pub mod aggregation;

use crate::abort;
use crate::commands::{data_path, Cli};
use crate::fieldutils::i32_to_felt;
use crate::graph::{utilities::vector_to_quantized, Model, ModelCircuit};
use crate::tensor::{Tensor, TensorType};
use clap::Parser;
use halo2_proofs::plonk::{
    create_proof, keygen_pk, keygen_vk, verify_proof, Circuit, ProvingKey, VerifyingKey,
};
use halo2_proofs::poly::commitment::{CommitmentScheme, Params, Prover, Verifier};
use halo2_proofs::poly::VerificationStrategy;
use halo2_proofs::transcript::{
    Blake2bRead, Blake2bWrite, Challenge255, TranscriptReadBuffer, TranscriptWriterBuffer,
};
use halo2_proofs::{arithmetic::FieldExt, dev::VerifyFailure};
use log::{error, info, trace};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::PathBuf;
use std::time::Instant;

/// The input tensor data and shape, and output data for the computational graph (model) as floats.
/// For example, the input might be the image data for a neural network, and the output class scores.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelInput {
    /// Inputs to the model / computational graph.
    pub input_data: Vec<Vec<f32>>,
    /// The shape of said inputs.
    pub input_shapes: Vec<Vec<usize>>,
    /// The expected output of the model (can be empty vectors if outputs are not being constrained).
    pub output_data: Vec<Vec<f32>>,
}

/// Defines the proof generated by a model / circuit suitably for serialization/deserialization.  
#[derive(Debug, Deserialize, Serialize)]
pub struct Proof {
    /// Public inputs to the model.
    pub public_inputs: Vec<Vec<i32>>,
    /// The generated proof, as a vector of bytes.
    pub proof: Vec<u8>,
}

impl Proof {
    /// Saves the Proof to a specified `proof_path`.
    pub fn save(&self, proof_path: &PathBuf) {
        let serialized = match serde_json::to_string(&self) {
            Ok(s) => s,
            Err(e) => {
                abort!("failed to convert proof json to string {:?}", e);
            }
        };

        let mut file = std::fs::File::create(proof_path).expect("create failed");
        file.write_all(serialized.as_bytes()).expect("write failed");
    }

    /// Load a json serialized proof from the provided path.
    pub fn load(proof_path: &PathBuf) -> Self {
        let mut file = match File::open(proof_path) {
            Ok(f) => f,
            Err(e) => {
                abort!("failed to open proof file {:?}", e);
            }
        };
        let mut data = String::new();
        match file.read_to_string(&mut data) {
            Ok(_) => {}
            Err(e) => {
                abort!("failed to read file {:?}", e);
            }
        };
        serde_json::from_str(&data).expect("JSON was not well-formatted")
    }
}

/// Helper function to print helpful error messages after verification has failed.
pub fn parse_prover_errors(f: &VerifyFailure) {
    match f {
        VerifyFailure::Lookup {
            name,
            location,
            lookup_index,
        } => {
            error!("lookup {:?} is out of range, try increasing 'bits' or reducing 'scale' ({} and lookup index {}).",
            name, location, lookup_index);
        }
        VerifyFailure::ConstraintNotSatisfied {
            constraint,
            location,
            cell_values: _,
        } => {
            error!("{} was not satisfied {}).", constraint, location);
        }
        VerifyFailure::ConstraintPoisoned { constraint } => {
            error!("constraint {:?} was poisoned", constraint);
        }
        VerifyFailure::Permutation { column, location } => {
            error!(
                "permutation did not preserve column cell value (try increasing 'scale') ({} {}).",
                column, location
            );
        }
        VerifyFailure::CellNotAssigned {
            gate,
            region,
            gate_offset,
            column,
            offset,
        } => {
            error!(
                "Unnassigned value in {} ({}) and {} ({:?}, {})",
                gate, region, gate_offset, column, offset
            );
        }
    }
}

/// Initialize the model circuit and quantize the provided float inputs from the provided `ModelInput`.
pub fn prepare_circuit_and_public_input<F: FieldExt>(
    data: &ModelInput,
) -> (ModelCircuit<F>, Vec<Tensor<i32>>) {
    let model = Model::from_arg();
    let out_scales = model.get_output_scales();
    let circuit = prepare_circuit(data);

    // quantize the supplied data using the provided scale.
    // the ordering here is important, we want the inputs to come before the outputs
    // as they are configured in that order as Column<Instances>
    let mut public_inputs = vec![];
    if model.visibility.input.is_public() {
        let mut res = data
            .input_data
            .iter()
            .enumerate()
            .map(|(idx, v)| {
                match vector_to_quantized(v, &Vec::from([v.len()]), 0.0, out_scales[idx]) {
                    Ok(q) => q,
                    Err(e) => {
                        abort!("failed to quantize vector {:?}", e);
                    }
                }
            })
            .collect();
        public_inputs.append(&mut res);
    }
    if model.visibility.output.is_public() {
        let mut res = data
            .output_data
            .iter()
            .enumerate()
            .map(|(idx, v)| {
                match vector_to_quantized(v, &Vec::from([v.len()]), 0.0, out_scales[idx]) {
                    Ok(q) => q,
                    Err(e) => {
                        abort!("failed to quantize vector {:?}", e);
                    }
                }
            })
            .collect();
        public_inputs.append(&mut res);
    }
    info!(
        "public inputs  lengths: {:?}",
        public_inputs
            .iter()
            .map(|i| i.len())
            .collect::<Vec<usize>>()
    );
    trace!("{:?}", public_inputs);
    (circuit, public_inputs)
}

/// Initialize the model circuit
pub fn prepare_circuit<F: FieldExt>(data: &ModelInput) -> ModelCircuit<F> {
    let args = Cli::parse();

    // quantize the supplied data using the provided scale.
    let inputs = data
        .input_data
        .iter()
        .zip(data.input_shapes.clone())
        .map(|(i, s)| match vector_to_quantized(i, &s, 0.0, args.scale) {
            Ok(q) => q,
            Err(e) => {
                abort!("failed to quantize vector {:?}", e);
            }
        })
        .collect();

    ModelCircuit::<F> {
        inputs,
        _marker: PhantomData,
    }
}

/// Deserializes the required inputs to a model at path `datapath` to a [ModelInput] struct.
pub fn prepare_data(datapath: String) -> ModelInput {
    let mut file = match File::open(data_path(datapath)) {
        Ok(t) => t,
        Err(e) => {
            abort!("failed to open data file {:?}", e);
        }
    };
    let mut data = String::new();
    match file.read_to_string(&mut data) {
        Ok(_) => {}
        Err(e) => {
            abort!("failed to read file {:?}", e);
        }
    };
    let data: ModelInput = serde_json::from_str(&data).expect("JSON was not well-formatted");

    data
}

/// Creates a [VerifyingKey] and [ProvingKey] for a [ModelCircuit] (`circuit`) with specific [CommitmentScheme] parameters (`params`).
pub fn create_keys<Scheme: CommitmentScheme, F: FieldExt + TensorType>(
    circuit: &ModelCircuit<F>,
    params: &'_ Scheme::ParamsProver,
) -> ProvingKey<Scheme::Curve>
where
    ModelCircuit<F>: Circuit<Scheme::Scalar>,
{
    //	Real proof
    let empty_circuit = circuit.without_witnesses();

    // Initialize the proving key
    let now = Instant::now();
    trace!("preparing VK");
    let vk = keygen_vk(params, &empty_circuit).expect("keygen_vk should not fail");
    info!("VK took {}", now.elapsed().as_secs());
    let now = Instant::now();
    let pk = keygen_pk(params, vk, &empty_circuit).expect("keygen_pk should not fail");
    info!("PK took {}", now.elapsed().as_secs());
    pk
}

/// a wrapper around halo2's create_proof
pub fn create_proof_model<
    'params,
    Scheme: CommitmentScheme,
    F: FieldExt + TensorType,
    P: Prover<'params, Scheme>,
>(
    circuit: &ModelCircuit<F>,
    public_inputs: &[Tensor<i32>],
    params: &'params Scheme::ParamsProver,
    pk: &ProvingKey<Scheme::Curve>,
) -> (Proof, Vec<Vec<usize>>)
where
    ModelCircuit<F>: Circuit<Scheme::Scalar>,
{
    let now = Instant::now();
    let mut transcript = Blake2bWrite::<_, Scheme::Curve, Challenge255<_>>::init(vec![]);
    let mut rng = OsRng;
    let pi_inner: Vec<Vec<Scheme::Scalar>> = public_inputs
        .iter()
        .map(|i| {
            i.iter()
                .map(|e| i32_to_felt::<Scheme::Scalar>(*e))
                .collect::<Vec<Scheme::Scalar>>()
        })
        .collect::<Vec<Vec<Scheme::Scalar>>>();
    let pi_inner = pi_inner
        .iter()
        .map(|e| e.deref())
        .collect::<Vec<&[Scheme::Scalar]>>();
    let instances: &[&[&[Scheme::Scalar]]] = &[&pi_inner];
    trace!("instances {:?}", instances);

    let dims = circuit.inputs.iter().map(|i| i.dims().to_vec()).collect();

    create_proof::<Scheme, P, _, _, _, _>(
        params,
        pk,
        &[circuit.clone()],
        instances,
        &mut rng,
        &mut transcript,
    )
    .expect("proof generation should not fail");
    let proof = transcript.finalize();
    info!("Proof took {}", now.elapsed().as_secs());

    let checkable_pf = Proof {
        public_inputs: public_inputs
            .iter()
            .map(|i| i.clone().into_iter().collect())
            .collect(),
        proof,
    };

    (checkable_pf, dims)
}

/// A wrapper around halo2's verify_proof
pub fn verify_proof_model<
    'params,
    F: FieldExt,
    V: Verifier<'params, Scheme>,
    Scheme: CommitmentScheme,
    Strategy: VerificationStrategy<'params, Scheme, V>,
>(
    proof: Proof,
    params: &'params Scheme::ParamsVerifier,
    vk: &VerifyingKey<Scheme::Curve>,
    strategy: Strategy,
) -> bool
where
    ModelCircuit<F>: Circuit<Scheme::Scalar>,
{
    let pi_inner: Vec<Vec<Scheme::Scalar>> = proof
        .public_inputs
        .iter()
        .map(|i| {
            i.iter()
                .map(|e| i32_to_felt::<Scheme::Scalar>(*e))
                .collect::<Vec<Scheme::Scalar>>()
        })
        .collect::<Vec<Vec<Scheme::Scalar>>>();
    let pi_inner = pi_inner
        .iter()
        .map(|e| e.deref())
        .collect::<Vec<&[Scheme::Scalar]>>();
    let instances: &[&[&[Scheme::Scalar]]] = &[&pi_inner];
    trace!("instances {:?}", instances);

    let now = Instant::now();
    let mut transcript = Blake2bRead::<_, _, Challenge255<_>>::init(&proof.proof[..]);

    let result =
        verify_proof::<Scheme, V, _, _, _>(params, vk, strategy, instances, &mut transcript)
            .is_ok();
    info!("verify took {}", now.elapsed().as_secs());
    result
}

/// Loads a [VerifyingKey] at `path`.
pub fn load_vk<Scheme: CommitmentScheme, F: FieldExt + TensorType>(
    path: PathBuf,
    params: &'_ Scheme::ParamsVerifier,
) -> VerifyingKey<Scheme::Curve>
where
    ModelCircuit<F>: Circuit<Scheme::Scalar>,
{
    info!("loading verification key from {:?}", path);
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            abort!("failed to load vk {}", e);
        }
    };
    let mut reader = BufReader::new(f);
    VerifyingKey::<Scheme::Curve>::read::<_, ModelCircuit<F>>(&mut reader, params).unwrap()
}

/// Loads the [CommitmentScheme::ParamsVerifier] at `path`.
pub fn load_params<Scheme: CommitmentScheme>(path: PathBuf) -> Scheme::ParamsVerifier {
    info!("loading params from {:?}", path);
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            abort!("failed to load params {}", e);
        }
    };
    let mut reader = BufReader::new(f);
    Params::<'_, Scheme::Curve>::read(&mut reader).unwrap()
}

/// Saves a [VerifyingKey] to `path`.
pub fn save_vk<Scheme: CommitmentScheme>(path: &PathBuf, vk: &VerifyingKey<Scheme::Curve>) {
    info!("saving verification key 💾");
    let f = File::create(path).unwrap();
    let mut writer = BufWriter::new(f);
    vk.write(&mut writer).unwrap();
    writer.flush().unwrap();
}

/// Saves [CommitmentScheme] parameters to `path`.
pub fn save_params<Scheme: CommitmentScheme>(path: &PathBuf, params: &'_ Scheme::ParamsVerifier) {
    info!("saving parameters 💾");
    let f = File::create(path).unwrap();
    let mut writer = BufWriter::new(f);
    params.write(&mut writer).unwrap();
    writer.flush().unwrap();
}