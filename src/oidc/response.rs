use axum::{
	body::Body,
	http::{Response, StatusCode, header},
	response::IntoResponse,
};
use oxide_auth::{endpoint::WebResponse, frontends::simple::request::Body as OAuthRequestBody};
use url::Url;

use super::OidcError;

/// A Web response that can be processed by the OIDC authentication flow before
/// being sent over.
#[derive(Default, Clone, Debug)]
pub struct OidcResponse {
	pub(crate) status: StatusCode,
	pub(crate) location: Option<Url>,
	pub(crate) www_authenticate: Option<String>,
	pub(crate) body: Option<OAuthRequestBody>,
	pub(crate) nonce: Option<String>,
}

impl IntoResponse for OidcResponse {
	fn into_response(self) -> Response<Body> {
		let csp_default_src = match self.nonce {
			| Some(nonce) => &format!("default-src 'nonce-{nonce}';"),
			| None => "default-src 'none';",
		};
		let csp_source = self.location.as_ref().map(|l| {
			format!(
				"http://{}:{}",
				l.domain().expect("some location domain"),
				l.port_or_known_default().expect("known protocol")
			)
		});
		let csp_form_action = match csp_source {
			| Some(s) => format!("form-action 'self' {s} http://localhost:* http://127.0.0.1:*;"),
			| None => "form-action 'self' http://localhost:* http://127.0.0.1:*;".to_string(),
		};
		// Adding localhost to the "form-action" directive lets Continuwuity
		// reply to private clients that call it _from_ localhost, any port.
		let csp_form_action = "form-action 'self' http://localhost:* http://127.0.0.1:*;";
		let content_csp = format!("{csp_default_src} {csp_form_action}");
		let content_type = match self.body {
			| Some(OAuthRequestBody::Json(_)) => "application/json",
			| _ => "text/html",
		};
		let mut response = Response::builder()
			.status(self.status)
			.header(header::CONTENT_TYPE, content_type)
			.header(header::CONTENT_SECURITY_POLICY, content_csp);
		if let Some(location) = self.location {
			response = response.header(header::LOCATION, location.as_str());
		}
		// Transform from OAuthRequestBody to String.
		let body_content = self.body.map(|b| b.as_str().to_owned()).unwrap_or_default();

		response.body(body_content.into()).unwrap()
	}
}

impl WebResponse for OidcResponse {
	type Error = OidcError;

	fn ok(&mut self) -> Result<(), Self::Error> {
		self.status = StatusCode::OK;

		Ok(())
	}

	/// A response which will redirect the user-agent to which the response is
	/// issued.
	fn redirect(&mut self, url: Url) -> Result<(), Self::Error> {
		self.status = StatusCode::FOUND;
		self.location = Some(url);

		Ok(())
	}

	/// Set the response status to 400.
	fn client_error(&mut self) -> Result<(), Self::Error> {
		self.status = StatusCode::BAD_REQUEST;

		Ok(())
	}

	/// Set the response status to 401 and add a `WWW-Authenticate` header.
	fn unauthorized(&mut self, header_value: &str) -> Result<(), Self::Error> {
		self.status = StatusCode::UNAUTHORIZED;
		self.www_authenticate = Some(header_value.to_owned());

		Ok(())
	}

	/// A pure text response with no special media type set.
	fn body_text(&mut self, text: &str) -> Result<(), Self::Error> {
		self.body = Some(OAuthRequestBody::Text(text.to_owned()));

		Ok(())
	}

	/// Json response data, with media type `aplication/json`.
	fn body_json(&mut self, data: &str) -> Result<(), Self::Error> {
		self.body = Some(OAuthRequestBody::Json(data.to_owned()));

		Ok(())
	}
}
