use axum::extract::{Query, State};
use conduwuit::Result;
use conduwuit_oidc::RevokeQuery;

/// # `GET /_matrix/client/unstable/org.matrix.msc4254/revoke`
///
/// Revoke a device by removing its token.
pub(crate) async fn revoke(
	State(services): State<crate::State>,
	Query(query): Query<RevokeQuery>,
) -> Result<()> {
	tracing::trace!("processing user's client revoke request: {query:#?}");
	let mut endpoint = services.oidc.endpoint.lock().await;

	endpoint.issuer.revoke_device(&query.token).await
}
