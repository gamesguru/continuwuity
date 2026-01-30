//! UIAA Fallback Authentication Web Endpoints
//!
//! Implements the fallback authentication flow as described in Matrix spec
//! section 4.9.1.4. This allows clients that don't have native UI for certain
//! auth types (like reCAPTCHA) to complete authentication via a web page.

use axum::{
	Form,
	extract::{Path, Query, State},
	response::{Html, IntoResponse},
};
use conduwuit::{Result, err};
use serde::Deserialize;

use crate::service::Services;

/// Query parameters for fallback auth GET request
#[derive(Debug, Deserialize)]
pub struct FallbackQuery {
	/// The UIAA session ID
	session: String,
}

/// Form data for fallback auth POST request (recaptcha)
#[derive(Debug, Deserialize)]
pub struct RecaptchaForm {
	/// The UIAA session ID
	session: String,
	/// The reCAPTCHA response token from Google
	#[serde(rename = "g-recaptcha-response")]
	recaptcha_response: String,
}

/// GET `/_matrix/client/v3/auth/m.login.recaptcha/fallback/web`
///
/// Serves an HTML page with the reCAPTCHA widget for clients that don't have
/// native reCAPTCHA UI.
pub async fn get_recaptcha_fallback(
	State(services): State<crate::State>,
	Query(query): Query<FallbackQuery>,
) -> Result<impl IntoResponse> {
	let session_id = &query.session;

	// Get the recaptcha site key from config
	let site_key = services
		.server
		.config
		.recaptcha_site_key
		.as_ref()
		.ok_or_else(|| err!(Request(Unknown("reCAPTCHA is not configured on this server"))))?;

	// Generate the HTML page with the reCAPTCHA widget
	let html = generate_recaptcha_html(site_key, session_id);

	Ok(Html(html))
}

/// POST `/_matrix/client/v3/auth/m.login.recaptcha/fallback/web`
///
/// Handles the reCAPTCHA form submission, validates with Google, and marks
/// the auth stage as complete.
pub async fn post_recaptcha_fallback(
	State(services): State<crate::State>,
	Form(form): Form<RecaptchaForm>,
) -> Result<impl IntoResponse> {
	let session_id = &form.session;
	let recaptcha_response = &form.recaptcha_response;

	// Get the secret key from config
	let secret_key = services
		.server
		.config
		.recaptcha_private_site_key
		.as_ref()
		.ok_or_else(|| err!(Request(Unknown("reCAPTCHA is not configured on this server"))))?;

	// Verify with Google
	let valid = services
		.uiaa
		.verify_recaptcha(recaptcha_response, secret_key)
		.await?;

	if !valid {
		// Return an error page
		let html = generate_error_html(session_id, "reCAPTCHA verification failed. Please try again.");
		return Ok(Html(html));
	}

	// Mark this stage as complete for the session
	services.uiaa.mark_stage_complete(session_id, "m.login.recaptcha");

	// Return success page that notifies the client
	let html = generate_success_html();
	Ok(Html(html))
}

/// Generates the HTML page with the reCAPTCHA widget
fn generate_recaptcha_html(site_key: &str, session_id: &str) -> String {
	format!(
		r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Authentication Required</title>
    <script src="https://www.google.com/recaptcha/api.js" async defer></script>
    <style>
        body {{
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
            display: flex;
            justify-content: center;
            align-items: center;
            min-height: 100vh;
            margin: 0;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
        }}
        .container {{
            background: white;
            padding: 2rem;
            border-radius: 12px;
            box-shadow: 0 10px 40px rgba(0,0,0,0.2);
            text-align: center;
            max-width: 400px;
        }}
        h1 {{
            color: #333;
            margin-bottom: 1rem;
            font-size: 1.5rem;
        }}
        p {{
            color: #666;
            margin-bottom: 1.5rem;
        }}
        .g-recaptcha {{
            display: inline-block;
            margin-bottom: 1rem;
        }}
        button {{
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            color: white;
            border: none;
            padding: 12px 32px;
            border-radius: 6px;
            font-size: 1rem;
            cursor: pointer;
            transition: transform 0.2s, box-shadow 0.2s;
        }}
        button:hover {{
            transform: translateY(-2px);
            box-shadow: 0 4px 12px rgba(102, 126, 234, 0.4);
        }}
    </style>
</head>
<body>
    <div class="container">
        <h1>Verify You're Human</h1>
        <p>Please complete the reCAPTCHA below to continue.</p>
        <form method="POST">
            <input type="hidden" name="session" value="{session_id}">
            <div class="g-recaptcha" data-sitekey="{site_key}"></div>
            <br>
            <button type="submit">Submit</button>
        </form>
    </div>
</body>
</html>"#,
		session_id = session_id,
		site_key = site_key
	)
}

/// Generates an error HTML page
fn generate_error_html(session_id: &str, error_message: &str) -> String {
	format!(
		r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Authentication Error</title>
    <script src="https://www.google.com/recaptcha/api.js" async defer></script>
    <style>
        body {{
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
            display: flex;
            justify-content: center;
            align-items: center;
            min-height: 100vh;
            margin: 0;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
        }}
        .container {{
            background: white;
            padding: 2rem;
            border-radius: 12px;
            box-shadow: 0 10px 40px rgba(0,0,0,0.2);
            text-align: center;
            max-width: 400px;
        }}
        h1 {{
            color: #e74c3c;
            margin-bottom: 1rem;
            font-size: 1.5rem;
        }}
        .error {{
            color: #e74c3c;
            margin-bottom: 1.5rem;
        }}
        p {{
            color: #666;
            margin-bottom: 1.5rem;
        }}
        .g-recaptcha {{
            display: inline-block;
            margin-bottom: 1rem;
        }}
        button {{
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            color: white;
            border: none;
            padding: 12px 32px;
            border-radius: 6px;
            font-size: 1rem;
            cursor: pointer;
            transition: transform 0.2s, box-shadow 0.2s;
        }}
        button:hover {{
            transform: translateY(-2px);
            box-shadow: 0 4px 12px rgba(102, 126, 234, 0.4);
        }}
    </style>
</head>
<body>
    <div class="container">
        <h1>Error</h1>
        <p class="error">{error_message}</p>
        <form method="POST">
            <input type="hidden" name="session" value="{session_id}">
            <div class="g-recaptcha" data-sitekey=""></div>
            <br>
            <button type="submit">Try Again</button>
        </form>
    </div>
</body>
</html>"#,
		session_id = session_id,
		error_message = error_message
	)
}

/// Generates success HTML that notifies the client via postMessage
fn generate_success_html() -> String {
	r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Authentication Successful</title>
    <style>
        body {
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
            display: flex;
            justify-content: center;
            align-items: center;
            min-height: 100vh;
            margin: 0;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
        }
        .container {
            background: white;
            padding: 2rem;
            border-radius: 12px;
            box-shadow: 0 10px 40px rgba(0,0,0,0.2);
            text-align: center;
            max-width: 400px;
        }
        h1 {
            color: #27ae60;
            margin-bottom: 1rem;
            font-size: 1.5rem;
        }
        .checkmark {
            font-size: 4rem;
            color: #27ae60;
            margin-bottom: 1rem;
        }
        p {
            color: #666;
        }
    </style>
</head>
<body>
    <div class="container">
        <div class="checkmark">✓</div>
        <h1>Verification Complete</h1>
        <p>You may now close this window and return to your application.</p>
    </div>
    <script>
        // Notify the parent window (the Matrix client) that auth succeeded
        if (window.opener) {
            window.opener.postMessage("m.login.recaptcha", "*");
        }
        // Also try parent for iframe-based clients
        if (window.parent && window.parent !== window) {
            window.parent.postMessage("m.login.recaptcha", "*");
        }
    </script>
</body>
</html>"#.to_string()
}
