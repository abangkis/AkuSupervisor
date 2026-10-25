//! Same-origin browser capability and service-control policy.
use super::{
    HttpRequest, IdempotencyStore, Response, error_response, json_response, parse_mutation_target,
    route_mutation_with_service_actor, valid_request_id,
};
use crate::adapters::{
    runtime_instance::RuntimeInstance, runtime_token::RuntimeToken,
    supervisor_shutdown::SupervisorShutdown,
};
use crate::application::{ControlAction, SupervisorControl};
use crate::domain::Actor;
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DashboardMutationRequest {
    request_id: String,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn route_dashboard_control(
    request: &HttpRequest,
    peer: SocketAddr,
    address: SocketAddr,
    token: &RuntimeToken,
    control: &dyn SupervisorControl,
    instance: &RuntimeInstance,
    supervisor_shutdown: &SupervisorShutdown,
    idempotency: &mut IdempotencyStore,
) -> Response {
    let mut response = dashboard_control_response(
        request,
        peer,
        address,
        token,
        control,
        instance,
        supervisor_shutdown,
        idempotency,
    );
    response.dashboard_security_headers = true;
    response
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn dashboard_control_response(
    request: &HttpRequest,
    peer: SocketAddr,
    address: SocketAddr,
    token: &RuntimeToken,
    control: &dyn SupervisorControl,
    instance: &RuntimeInstance,
    supervisor_shutdown: &SupervisorShutdown,
    idempotency: &mut IdempotencyStore,
) -> Response {
    let Some(host) = request.host.as_deref() else {
        return error_response(404, "not_found", "route not found");
    };
    let valid_host = [
        format!("127.0.0.1:{}", address.port()),
        format!("localhost:{}", address.port()),
        format!("[::1]:{}", address.port()),
    ]
    .iter()
    .any(|allowed| host == allowed);
    if !peer.ip().is_loopback() || !address.ip().is_loopback() || !valid_host {
        return error_response(404, "not_found", "route not found");
    }
    let same_origin = format!("http://{host}");
    if request
        .origin
        .as_deref()
        .is_some_and(|origin| origin != same_origin)
    {
        return error_response(
            403,
            "origin_forbidden",
            "same-origin dashboard request required",
        );
    }
    let dashboard_token =
        token.derive_scoped(&format!("dashboard-v1:{}:{address}", instance.instance_id));
    if request.target == "/dashboard/v1/session" {
        if request.method != "GET" {
            return error_response(405, "method_not_allowed", "GET required");
        }
        // A custom header prevents cross-site simple requests. No CORS is enabled.
        if request.dashboard_request.as_deref() != Some("1") {
            return error_response(
                403,
                "dashboard_header_required",
                "dashboard request header required",
            );
        }
        return json_response(
            200,
            &json!({
                "token": dashboard_token.expose_for_authorization_header(),
                "instanceId": instance.instance_id,
            }),
        );
    }
    let Some(target) = request.target.strip_prefix("/dashboard") else {
        return error_response(404, "not_found", "route not found");
    };
    let Some((_service_id, action)) = parse_mutation_target(target) else {
        return error_response(404, "not_found", "route not found");
    };
    if !matches!(
        action,
        ControlAction::Start | ControlAction::Stop | ControlAction::Restart
    ) {
        return error_response(404, "not_found", "route not found");
    }
    if request.method != "POST" {
        return error_response(405, "method_not_allowed", "POST required");
    }
    if request.origin.as_deref() != Some(same_origin.as_str()) {
        return error_response(
            403,
            "origin_required",
            "same-origin dashboard request required",
        );
    }
    if request.content_type.as_deref() != Some("application/json") {
        return error_response(415, "json_required", "application/json required");
    }
    if !request
        .dashboard_token
        .as_deref()
        .is_some_and(|candidate| dashboard_token.bearer_matches(candidate))
    {
        return error_response(401, "unauthorized", "valid dashboard token required");
    }
    let mutation: DashboardMutationRequest = match serde_json::from_slice(&request.body) {
        Ok(mutation) => mutation,
        Err(_) => {
            return error_response(
                400,
                "invalid_request",
                "requestId is required; no other fields are accepted",
            );
        }
    };
    if !valid_request_id(&mutation.request_id) || mutation.request_id.len() > 118 {
        return error_response(
            400,
            "invalid_request_id",
            "requestId must be 1-118 URL-safe ASCII characters",
        );
    }
    // Reuse authorization, user priority, validation, audit and idempotency semantics.
    // The browser capability cannot reach shutdown, registry or MCP routes.
    let forwarded = HttpRequest {
        method: "POST".to_owned(),
        target: target.to_owned(),
        authorization: Some(format!(
            "Bearer {}",
            token.expose_for_authorization_header()
        )),
        body: serde_json::to_vec(&json!({
            "actor": "user",
            "reason": "Dashboard service control",
            "requestId": format!("dashboard-{}", mutation.request_id),
        }))
        .expect("serialize dashboard mutation"),
        ..request.clone()
    };
    route_mutation_with_service_actor(
        &forwarded,
        token,
        control,
        None,
        supervisor_shutdown,
        instance,
        idempotency,
        Some(Actor::UserUi),
    )
}

pub(super) fn dashboard_response(status: u16, body: &[u8], content_type: &'static str) -> Response {
    Response {
        status,
        body: body.to_vec(),
        content_type: Some(content_type),
        dashboard_security_headers: true,
    }
}

pub(super) fn dashboard_request_is_loopback(peer: SocketAddr, host: Option<&str>) -> bool {
    peer.ip().is_loopback() && host.is_some_and(is_loopback_host)
}

pub(super) fn is_loopback_host(authority: &str) -> bool {
    let authority = authority.trim();
    if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((address, suffix)) = bracketed.split_once(']') else {
            return false;
        };
        if !suffix.is_empty()
            && !suffix.strip_prefix(':').is_some_and(|port| {
                !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return false;
        }
        return address
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    }

    let host = authority.split_once(':').map_or(authority, |(host, port)| {
        if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            ""
        } else {
            host
        }
    });
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}
