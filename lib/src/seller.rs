use crate::traits::ChainProvider;
use crate::{
    keypair_from_bytes, CipherHost, PairingEngine, ProjectiveCurve, ZkConfig, ZkPropertyVerifier,
};
use anyhow::anyhow;
use ecdsa_fun::adaptor::{Adaptor, EncryptedSignature, HashTranscript};
use ethers::prelude::*;
use futures::channel::{mpsc, oneshot};
use num_bigint::BigInt;
use rand::{CryptoRng, Rng};
use rand_chacha::ChaCha20Rng;
use secp256kfun::marker::{Mark, Normal};
use secp256kfun::nonce::Deterministic;
use secp256kfun::{g, Point, Scalar, G};
use sha2::Sha256;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::zk_encryption::ZkEncryption;
use circuits::{ark_to_bytes, bytes_to_plaintext_chunks, encryption};

pub struct Seller<TChainProvider, TCipherHost> {
    cfg: SellerConfig,
    adaptor: Adaptor<HashTranscript<Sha256, ChaCha20Rng>, Deterministic<Sha256>>,
    chain: TChainProvider,
    cipher_host: TCipherHost,
    wallet: crate::LocalWallet,
    from_buyers: mpsc::Receiver<SellerMsg>,
    one_time_keys: HashMap<Address, Scalar>,
    decryption_key: Option<Vec<u8>>,
    property_verifier: ZkPropertyVerifier,
    key_encryption: ZkEncryption,
}

pub enum SellerMsg {
    /// Step 0: Alice encrypts data and generates Proof-of-Encryption (PoE);
    /// Bob requests ciphertext and verifies proof.
    Step0 {
        resp_tx: oneshot::Sender<anyhow::Result<Step0Msg>>,
    },
    /// Step 1:Alice generates new key pair, encrypt data decryption key with it, and sends public key and ciphertext to Bob.
    Step1 {
        address: Address,
        resp_tx: oneshot::Sender<anyhow::Result<Step1Msg>>,
    },
    /// Step 3: Alice decrypts this signature and publishes it, ie. get paid
    Step3 {
        pub_key: Point,
        enc_sig: EncryptedSignature,
        resp_tx: oneshot::Sender<anyhow::Result<H256>>,
    },
}

pub struct Step0Msg {
    pub ciphertext: Vec<u8>,
    pub proof_of_encryption: Vec<u8>,
}

pub struct Step1Msg {
    pub ciphertext: Vec<u8>,
    pub proof_of_encryption: Vec<u8>,
    pub data_pk: Point,
    pub seller_address: Address,
}

#[derive(Clone, Debug)]
pub struct SellerConfig {
    pub price: f64,
    pub cache_dir: PathBuf,
    pub zk: ZkConfig,
}

impl<TChainProvider: ChainProvider, TCipherHost: CipherHost> Seller<TChainProvider, TCipherHost> {
    pub fn new(
        cfg: SellerConfig,
        chain: TChainProvider,
        cipher_host: TCipherHost,
        wallet: crate::LocalWallet,
    ) -> anyhow::Result<(Self, mpsc::Sender<SellerMsg>)> {
        let nonce_gen = Deterministic::<Sha256>::default();
        let adaptor = Adaptor::<HashTranscript<Sha256, ChaCha20Rng>, _>::new(nonce_gen);
        let (to_seller, from_buyers) = mpsc::channel(1);
        let decryption_key =
            fs::read(cfg.cache_dir.join("decryption_key")).map_or(None, |b| Some(b));
        let property_verifier = ZkPropertyVerifier::new(
            &cfg.zk.prop_verifier_dir,
            cfg.zk.circom_params.clone(),
            encryption::Parameters::default_multi(cfg.zk.data_encryption_limit),
        );
        let key_encryption = ZkEncryption::new(&cfg.zk.key_encryption_dir, Default::default());
        Ok((
            Self {
                cfg,
                adaptor,
                one_time_keys: HashMap::default(),
                chain,
                cipher_host,
                from_buyers,
                wallet,
                decryption_key,
                property_verifier,
                key_encryption,
            },
            to_seller,
        ))
    }

    pub async fn step0_setup(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        let (sk, pk) = self.property_verifier.keygen(&mut rand::thread_rng())?;

        let sk_bytes =
            ark_to_bytes(sk).map_err(|e| anyhow!("error encoding elgamal secret key: {e}"))?;

        fs::create_dir_all(&self.cfg.cache_dir).expect("expected dir to be created");
        fs::write(
            self.cfg.cache_dir.join("decryption_key"),
            self.decryption_key.insert(sk_bytes),
        )
        .map_err(|e| anyhow!("error caching decryption key: {e}"))?;

        let addt_vals = HashMap::new();
        let (encrypted_data, proof_of_encryption) =
            self.property_verifier.assess_property_and_encrypt(
                data,
                pk,
                addt_vals.into_iter(),
                &mut rand::thread_rng(),
            )?;

        let _ = self
            .cipher_host
            .write(encrypted_data, proof_of_encryption)
            .await;

        Ok(())
    }

    pub async fn run(mut self) {
        loop {
            if let Some(msg) = self.from_buyers.next().await {
                match msg {
                    SellerMsg::Step0 { resp_tx } => {
                        let _ = resp_tx.send(self.cipher_host.read().await.map(
                            |(ciphertext, proof_of_encryption)| Step0Msg {
                                ciphertext,
                                proof_of_encryption,
                            },
                        )); // todo: DoS defense needed.
                    }
                    SellerMsg::Step1 { address, resp_tx } => {
                        let (elgamal_pk, data_sk, data_pk) = self
                            .key_encryption
                            .keygen_derive(&mut rand::thread_rng())
                            .expect("expected generation to succeed or infinite looped");
                        let _ = self.one_time_keys.insert(address, data_sk);
                        let seller_address = self.chain.address_from_pk(self.wallet.pub_key());
                        let plaintext = self
                            .decryption_key
                            .as_ref()
                            .expect("decryption key was expected");
                        if let Err(_) = resp_tx.send(
                            self.key_encryption
                                .encrypt(plaintext, elgamal_pk, &mut rand::thread_rng())
                                .map(|(ciphertext, proof_of_encryption)| Step1Msg {
                                    ciphertext,
                                    proof_of_encryption,
                                    data_pk,
                                    seller_address,
                                }),
                        ) {
                            self.one_time_keys.remove(&address); // todo: DoS defense needed.
                        }
                    }
                    SellerMsg::Step3 {
                        pub_key,
                        enc_sig,
                        resp_tx,
                    } => {
                        let local_address = self.chain.address_from_pk(self.wallet.pub_key());
                        let address = self.chain.address_from_pk(&pub_key);
                        let decryption_key = match self.one_time_keys.entry(address) {
                            Entry::Occupied(e) => e.remove(),
                            Entry::Vacant(_) => {
                                let _ = resp_tx.send(Err(anyhow!("unknown address")));
                                continue;
                            }
                        };

                        let (pay_tx, tx_hash) = self
                            .chain
                            .compose_tx(address, local_address, self.cfg.price)
                            .unwrap();

                        let one_time_pk = g!(decryption_key * G).mark::<Normal>();
                        if !self.adaptor.verify_encrypted_signature(
                            &pub_key,
                            &one_time_pk,
                            tx_hash.as_fixed_bytes(),
                            &enc_sig,
                        ) {
                            let _ = resp_tx.send(Err(anyhow!("invalid adaptor signature")));
                            continue;
                        }
                        let decrypted_sig =
                            self.adaptor.decrypt_signature(&decryption_key, enc_sig);

                        let _ = resp_tx.send(self.chain.sent_signed(pay_tx, &decrypted_sig).await);
                    }
                }
            }
        }
    }
}
