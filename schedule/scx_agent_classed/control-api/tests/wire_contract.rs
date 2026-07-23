use scx_agent_classed_control::{
    validate_comm, ControlOp, ControlRequest, RuleClass, RuleState, CONTROL_VERSION,
    MAX_COMM_BYTES,
};

#[test]
fn comm_validation_uses_linux_visible_byte_limit() {
    assert!(validate_comm("worker").is_ok());
    assert!(validate_comm(&"a".repeat(MAX_COMM_BYTES)).is_ok());
    assert!(validate_comm("").is_err());
    assert!(validate_comm(&"a".repeat(MAX_COMM_BYTES + 1))
        .unwrap_err()
        .contains("at most 15"));
    assert!(validate_comm(&"\u{00e9}".repeat(8)).is_err());
    assert!(validate_comm("line\nbreak").is_err());
}

#[test]
fn operation_shape_is_validated_at_the_protocol_boundary() {
    let request = ControlRequest {
        version: CONTROL_VERSION,
        request_id: "request-1".into(),
        op: ControlOp::CompareAndSetRule,
        comm: Some("worker".into()),
        comms: None,
        expected: Some(RuleState::absent()),
        desired: Some(RuleState::present(RuleClass::Latency)),
    };
    assert!(request.validate().is_ok());

    let mut invalid = request.clone();
    invalid.comms = Some(vec!["worker".into()]);
    assert!(invalid.validate().is_err());

    let mut wrong_version = request;
    wrong_version.version += 1;
    assert!(wrong_version.validate().is_err());
}
