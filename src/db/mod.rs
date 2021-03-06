mod schema;
use revault_net::{
    bitcoin::{
        consensus::encode,
        secp256k1::{PublicKey, Signature},
        OutPoint, Transaction as BitcoinTransaction, Txid,
    },
    message::server::Sigs,
};
use schema::SCHEMA;

use std::{collections::BTreeMap, fmt};

use tokio_postgres::{types::Type, Client, NoTls};

#[derive(Debug)]
pub enum DbError {
    /// An error originating from the Postgres backend
    Postgres(tokio_postgres::Error),
    /// Trying to insert the same data twice
    Duplicate,
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Postgres(e) => write!(f, "{}", e),
            Self::Duplicate => write!(f, "Trying to insert a duplicated entry"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<tokio_postgres::Error> for DbError {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::Postgres(e)
    }
}

async fn establish_connection(
    config: &tokio_postgres::Config,
) -> Result<Client, tokio_postgres::Error> {
    let (client, connection) = config.connect(NoTls).await?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            log::error!("Database connection error: {}", e);
        }
    });

    Ok(client)
}

pub async fn maybe_create_db(config: &tokio_postgres::Config) -> Result<(), tokio_postgres::Error> {
    let client = establish_connection(config).await?;

    client.batch_execute(SCHEMA).await?;

    Ok(())
}

pub async fn store_sig(
    config: &tokio_postgres::Config,
    txid: Txid,
    pubkey: PublicKey,
    signature: Signature,
) -> Result<(), DbError> {
    let client = establish_connection(config).await?;
    let sig = signature.serialize_der();

    // Make sure it's not here already
    let statement = client
        .prepare_typed(
            "SELECT signature FROM signatures WHERE signature = $1",
            &[Type::BYTEA],
        )
        .await?;
    if !client.query(&statement, &[&sig.as_ref()]).await?.is_empty() {
        return Err(DbError::Duplicate);
    }

    let statement = client
        .prepare_typed(
            "INSERT INTO signatures (txid, pubkey, signature) VALUES ($1, $2, $3)",
            &[Type::BYTEA, Type::BYTEA, Type::BYTEA],
        )
        .await?;
    client
        .execute(
            &statement,
            &[&txid.as_ref(), &pubkey.serialize().as_ref(), &sig.as_ref()],
        )
        .await?;

    Ok(())
}

pub async fn fetch_sigs(
    config: &tokio_postgres::Config,
    txid: Txid,
) -> Result<Sigs, tokio_postgres::Error> {
    let client = establish_connection(config).await?;
    let mut signatures: BTreeMap<PublicKey, Signature> = BTreeMap::new();

    let statement = client
        .prepare_typed(
            "SELECT pubkey, signature FROM signatures WHERE txid = $1",
            &[Type::BYTEA],
        )
        .await?;
    for row in client.query(&statement, &[&txid.as_ref()]).await? {
        let pubkey: &[u8] = row.get(0);
        let pubkey = PublicKey::from_slice(&pubkey).expect("We input a compressed pubkey");
        let sig: Vec<u8> = row.get(1);

        signatures.insert(
            pubkey,
            Signature::from_der(&sig).expect("We input to_der()"),
        );
    }

    Ok(Sigs { signatures })
}

pub async fn store_spend_tx(
    config: &tokio_postgres::Config,
    outpoints: &Vec<OutPoint>,
    transaction: BitcoinTransaction,
) -> Result<(), tokio_postgres::Error> {
    let mut client = establish_connection(config).await?;
    let bitcoin_txid = encode::serialize(&transaction.txid());
    let bitcoin_tx = encode::serialize(&transaction);

    // In a single transaction,
    let db_tx = client.transaction().await?;

    // insert the Spend transaction,
    let statement = db_tx
        .prepare_typed(
            "INSERT INTO spend_txs (txid, transaction) VALUES ($1, $2) \
             ON CONFLICT DO NOTHING", // FIXME: we should make the error explicit
            &[Type::BYTEA, Type::BYTEA],
        )
        .await?;
    db_tx
        .execute(&statement, &[&bitcoin_txid, &bitcoin_tx])
        .await?;

    // as well as all vault outpoints it refers to
    for outpoint in outpoints.iter() {
        let statement = db_tx.prepare_typed(
        "INSERT INTO spend_outpoints (deposit_txid, deposit_vout, spend_txid) VALUES ($1, $2, $3) \
         ON CONFLICT (deposit_txid, deposit_vout) DO UPDATE \
         SET deposit_txid = EXCLUDED.deposit_txid, \
             deposit_vout = EXCLUDED.deposit_vout, \
             spend_txid = EXCLUDED.spend_txid",
        &[Type::BYTEA, Type::INT4, Type::BYTEA]
        ).await?;
        db_tx
            .execute(
                &statement,
                &[
                    &outpoint.txid.as_ref(),
                    &(outpoint.vout as i32),
                    &bitcoin_txid,
                ],
            )
            .await?;
    }

    db_tx.commit().await
}

pub async fn fetch_spend_tx(
    config: &tokio_postgres::Config,
    outpoint: OutPoint,
) -> Result<Option<BitcoinTransaction>, tokio_postgres::Error> {
    let client = establish_connection(config).await?;

    let statement = client
        .prepare_typed(
            "SELECT transaction FROM spend_txs as txs \
             INNER JOIN spend_outpoints as ops ON txs.txid = ops.spend_txid \
             WHERE ops.deposit_txid = $1 AND ops.deposit_vout = $2",
            &[Type::BYTEA, Type::INT4],
        )
        .await?;
    let spend_tx = client
        .query(
            &statement,
            &[&outpoint.txid.as_ref(), &(outpoint.vout as i32)],
        )
        .await?
        .get(0)
        .map(|row| row.get::<_, Vec<u8>>(0));

    Ok(spend_tx.map(|tx| encode::deserialize(&tx).expect("Added to DB with serialize()")))
}
