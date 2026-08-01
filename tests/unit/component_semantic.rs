use super::*;

#[test]
fn elaboration_result_distinguishes_pending_and_completed_work() {
    assert_eq!(Elaboration::complete(7_u8), Elaboration::Complete(7));
    assert_eq!(Elaboration::<u8>::awaiting(), Elaboration::Awaiting);
}

#[test]
fn elaborator_framework_diagnostics_are_preserved() {
    let error: ElaboratorError<NoDiagnostic> = FrameworkDiagnostic::MissingInterpretation.into();
    assert!(matches!(
        error,
        ElaboratorError::Framework(FrameworkDiagnostic::MissingInterpretation)
    ));
}
