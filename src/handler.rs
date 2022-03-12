use hyper::{header::WWW_AUTHENTICATE, Body, Method, Request, Response, StatusCode};
use std::{future::Future, path::PathBuf, sync::Arc};

use crate::{
    basic_auth, compression, control_headers, cors, error_page, fallback_page, security_headers,
    static_files,
};
use crate::{Error, Result};

/// It defines options for a request handler.
pub struct RequestHandlerOpts {
    pub root_dir: PathBuf,
    pub compression: bool,
    pub dir_listing: bool,
    pub dir_listing_order: u8,
    pub cors: Option<cors::Configured>,
    pub security_headers: bool,
    pub cache_control_headers: bool,
    pub page404: String,
    pub page50x: String,
    pub page_fallback: Option<String>,
    pub basic_auth: String,
}

/// It defines the main request handler used by the Hyper service request.
pub struct RequestHandler {
    pub opts: Arc<RequestHandlerOpts>,
}

impl RequestHandler {
    /// Main entry point for incoming requests.
    pub fn handle<'a>(
        &'a self,
        req: &'a mut Request<Body>,
    ) -> impl Future<Output = Result<Response<Body>, Error>> + Send + 'a {
        let method = req.method();
        let headers = req.headers();
        let uri = req.uri();

        let root_dir = &self.opts.root_dir;
        let uri_path = uri.path();
        let uri_query = uri.query();
        let dir_listing = self.opts.dir_listing;
        let dir_listing_order = self.opts.dir_listing_order;

        let mut cors_headers: Option<http::HeaderMap> = None;

        async move {
            // Check for disallowed HTTP methods and reject request accordently
            if !(method == Method::GET || method == Method::HEAD || method == Method::OPTIONS) {
                return error_page::error_response(
                    method,
                    &StatusCode::METHOD_NOT_ALLOWED,
                    &self.opts.page404,
                    &self.opts.page50x,
                );
            }

            // CORS
            if let Some(cors) = &self.opts.cors {
                match cors.check_request(method, headers) {
                    Ok((headers, state)) => {
                        tracing::debug!("cors state: {:?}", state);
                        cors_headers = Some(headers);
                    }
                    Err(err) => {
                        tracing::error!("cors error kind: {:?}", err);
                        return error_page::error_response(
                            method,
                            &StatusCode::FORBIDDEN,
                            &self.opts.page404,
                            &self.opts.page50x,
                        );
                    }
                };
            }

            // `Basic` HTTP Authorization Schema
            if !self.opts.basic_auth.is_empty() {
                if let Some((user_id, password)) = self.opts.basic_auth.split_once(':') {
                    if let Err(err) = basic_auth::check_request(headers, user_id, password) {
                        tracing::warn!("basic authentication failed {:?}", err);
                        let mut resp = error_page::error_response(
                            method,
                            &StatusCode::UNAUTHORIZED,
                            &self.opts.page404,
                            &self.opts.page50x,
                        )?;
                        resp.headers_mut().insert(
                            WWW_AUTHENTICATE,
                            "Basic realm=\"Static Web Server\", charset=\"UTF-8\""
                                .parse()
                                .unwrap(),
                        );
                        return Ok(resp);
                    }
                } else {
                    tracing::error!("invalid basic authentication `user_id:password` pairs");
                    return error_page::error_response(
                        method,
                        &StatusCode::INTERNAL_SERVER_ERROR,
                        &self.opts.page404,
                        &self.opts.page50x,
                    );
                }
            }

            // Static files
            match static_files::handle(
                method,
                headers,
                root_dir,
                uri_path,
                uri_query,
                dir_listing,
                dir_listing_order,
            )
            .await
            {
                Ok(mut resp) => {
                    // Append CORS headers if they are present
                    if let Some(cors_headers) = cors_headers {
                        if !cors_headers.is_empty() {
                            for (k, v) in cors_headers.iter() {
                                resp.headers_mut().insert(k, v.to_owned());
                            }
                            resp.headers_mut().remove(http::header::ALLOW);
                        }
                    }

                    // Auto compression based on the `Accept-Encoding` header
                    if self.opts.compression {
                        resp = match compression::auto(method, headers, resp) {
                            Ok(res) => res,
                            Err(err) => {
                                tracing::error!("error during body compression: {:?}", err);
                                return error_page::error_response(
                                    method,
                                    &StatusCode::INTERNAL_SERVER_ERROR,
                                    &self.opts.page404,
                                    &self.opts.page50x,
                                );
                            }
                        };
                    }

                    // Append `Cache-Control` headers for web assets
                    if self.opts.cache_control_headers {
                        control_headers::append_headers(uri_path, &mut resp);
                    }

                    // Append security headers
                    if self.opts.security_headers {
                        security_headers::append_headers(&mut resp);
                    }

                    Ok(resp)
                }
                Err(status) => {
                    if let Some(response) =
                        fallback_page::fallback_response(method, &status, &self.opts.page_fallback)
                    {
                        return Ok(response);
                    }
                    error_page::error_response(
                        method,
                        &status,
                        &self.opts.page404,
                        &self.opts.page50x,
                    )
                }
            }
        }
    }
}
