# Nova: High-speed recursive arguments from folding schemes

> [!NOTE]
> This repository is a fork of the original hosted at [https://github.com/microsoft/nova](https://github.com/microsoft/nova). It's an incubator for experimenting with more advanced variants of the original software and working out the kinks in them.

> [!IMPORTANT]
> This fork is always maintained as up-to-date as the original repository. Occasionally, this repository backports its own contributions to the original, after an incubation time. Note that until back-ported, the changes in the present repository have not undergone the same level of review.


Nova is a high-speed recursive SNARK (a SNARK is type cryptographic proof system that enables a prover to prove a mathematical statement to a verifier with a short proof and succinct verification, and a recursive SNARK enables producing proofs that prove statements about prior proofs). 

More precisely, Nova achieves [incrementally verifiable computation (IVC)](https://iacr.org/archive/tcc2008/49480001/49480001.pdf), a powerful cryptographic primitive that allows a prover to produce a proof of correct execution of a "long running" sequential computations in an incremental fashion. For example, IVC enables the following: The prover takes as input a proof $\pi_i$ proving the the first $i$ steps of its computation and then update it to produce a proof $\pi_{i+1}$ proving the correct execution of the first $i + 1$ steps. Crucially, the prover's work to update the proof does not depend on the number of steps executed thus far, and the verifier's work to verify a proof does not grow with the number of steps in the computation. IVC schemes including Nova have a wide variety of applications such as Rollups, verifiable delay functions (VDFs), succinct blockchains, incrementally verifiable versions of [verifiable state machines](https://eprint.iacr.org/2020/758.pdf), and, more generally, proofs of (virtual) machine executions (e.g., EVM, RISC-V). 

A distinctive aspect of Nova is that it is the simplest recursive proof system in the literature, yet it provides the fastest prover. Furthermore, it achieves the smallest verifier circuit (a key metric to minimize in this context): the circuit is constant-sized and its size is about 10,000 multiplication gates. Nova is constructed from a simple primitive called a *folding scheme*, a cryptographic primitive that reduces the task of checking two NP statements into the task of checking a single NP statement. 

## Library details
This repository provides `nova-snark,` a Rust library implementation of Nova on a cycle of elliptic curves. The code currently supports Pallas/Vesta (i.e., Pasta curves), BN254/Grumpkin, and secp/secq elliptic curve cycles. One can use Nova with other elliptic curve cycles by providing an implementation of Nova's traits for those curves (e.g., see `src/provider/mod.rs`).

We also implement a SNARK, based on [Spartan](https://eprint.iacr.org/2019/550.pdf), to compress IVC proofs produced by Nova.

At its core, Nova relies on a commitment scheme for vectors, and IVC proof compression with Spartan relies on interpreting commitments to vectors as commitments to multilinear polynomials and prove evaluations of committed polynomials. 

The current code implements two commitment schemes: (1) Pedersen commitments with IPA-based evaluation argument (supported on all the three curve cycles); and (2) multilinear KZG commitments and evaluation argument (supported on curves with support for pairings e.g., BN254). For more details, please see the test `test_ivc_nontrivial_with_compression`. The multilinear KZG instantiation requires a universal trusted setup ("powers of tau"). In the `setup` method in `src/provider/mlkzg.rs`, one can load group elements produced in an existing KZG trusted setup (that was created for other proof systems based on univariate polynomials such as Plonk or Marlin or variants), but the library does not currently do so. 

## Tests and examples
To run tests (we recommend the release mode to drastically shorten run times):
```text
cargo test --release
```

To run example:
```text
cargo run --release --example minroot
```

## Specs

- [SuperNova](./notes/supernova.md)

## References
The following paper, which appeared at CRYPTO 2022, provides details of the Nova proof system and a proof of security:

[Nova: Recursive Zero-Knowledge Arguments from Folding Schemes](https://eprint.iacr.org/2021/370) \
Abhiram Kothapalli, Srinath Setty, and Ioanna Tzialla \
CRYPTO 2022

For efficiency, our implementation of the Nova proof system is instantiated over a cycle of elliptic curves. The following paper specifies that instantiation and provides a proof of security:

[Revisiting the Nova Proof System on a Cycle of Curves](https://eprint.iacr.org/2023/969) \
Wilson Nguyen, Dan Boneh, and Srinath Setty \
IACR ePrint 2023/969

## Acknowledgments
See the contributors list [here](https://github.com/microsoft/Nova/graphs/contributors)
