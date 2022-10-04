use anyhow::Result;
use cid::Cid;
use libipld_cbor::DagCborCodec;
use noosphere_storage::interface::BlockStore;

use crate::{
    data::AuthorizationIpld,
    view::{AllowedUcans, RevokedUcans},
};

pub struct Authorization<S: BlockStore> {
    cid: Cid,
    store: S,
}

impl<S> Authorization<S>
where
    S: BlockStore,
{
    pub fn cid(&self) -> &Cid {
        &self.cid
    }

    pub fn at(cid: &Cid, store: &S) -> Self {
        Authorization {
            cid: *cid,
            store: store.clone(),
        }
    }

    pub async fn try_at_or_empty(cid: Option<&Cid>, store: &mut S) -> Result<Authorization<S>> {
        Ok(match cid {
            Some(cid) => Self::at(cid, store),
            None => Self::try_empty(store).await?,
        })
    }

    pub async fn try_empty(store: &mut S) -> Result<Self> {
        let ipld = AuthorizationIpld::try_empty(store).await?;
        let cid = store.save::<DagCborCodec, _>(ipld).await?;

        Ok(Authorization {
            cid,
            store: store.clone(),
        })
    }

    pub async fn try_get_allowed_ucans(&self) -> Result<AllowedUcans<S>> {
        let ipld = self
            .store
            .load::<DagCborCodec, AuthorizationIpld>(&self.cid)
            .await?;

        AllowedUcans::try_at_or_empty(Some(&ipld.allowed), &mut self.store.clone()).await
    }

    pub async fn try_get_revoked_ucans(&self) -> Result<RevokedUcans<S>> {
        let ipld = self
            .store
            .load::<DagCborCodec, AuthorizationIpld>(&self.cid)
            .await?;

        RevokedUcans::try_at_or_empty(Some(&ipld.revoked), &mut self.store.clone()).await
    }
}
