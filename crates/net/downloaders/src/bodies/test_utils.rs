//! Test helper impls for generating bodies

#![allow(dead_code)]

use reth_db::{tables, DatabaseEnvIAVL};
use reth_db_api::{database::Database, transaction::DbTxMut};
use reth_network_p2p::bodies::response::BlockResponse;
use reth_primitives::{Block, BlockBody, SealedBlock, SealedHeader, B256};
use std::collections::HashMap;

pub(crate) fn zip_blocks<'a>(
    headers: impl Iterator<Item = &'a SealedHeader>,
    bodies: &mut HashMap<B256, BlockBody>,
) -> Vec<BlockResponse> {
    headers
        .into_iter()
        .map(|header| {
            let body = bodies.remove(&header.hash()).expect("body exists");
            if header.is_empty() {
                BlockResponse::Empty(header.clone())
            } else {
                BlockResponse::Full(SealedBlock {
                    header: header.clone(),
                    body: body.transactions,
                    ommers: body.ommers,
                    withdrawals: body.withdrawals,
                    requests: body.requests,
                })
            }
        })
        .collect()
}

pub(crate) fn create_raw_bodies(
    headers: impl IntoIterator<Item = SealedHeader>,
    bodies: &mut HashMap<B256, BlockBody>,
) -> Vec<Block> {
    headers
        .into_iter()
        .map(|header| {
            let body = bodies.remove(&header.hash()).expect("body exists");
            body.create_block(header.unseal())
        })
        .collect()
}

#[inline]
pub(crate) fn insert_headers(db: &DatabaseEnvIAVL, headers: &[SealedHeader]) {
    db.update(|tx| {
        for header in headers {
            tx.put::<tables::CanonicalHeaders>(header.number, header.hash()).unwrap();
            tx.put::<tables::Headers>(header.number, header.clone().unseal()).unwrap();
        }
    })
    .expect("failed to commit")
}
