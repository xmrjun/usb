use std::time::Duration;

use crate::backend::{BackendError, Cancellation, EjectResult, VetoKind};

const MAX_ATTEMPTS: u8 = 3;
const RETRY_DELAY: Duration = Duration::from_millis(500);
pub const SLOW_REQUEST_THRESHOLD: Duration = Duration::from_secs(2);
pub const RETRY_TIME_BUDGET: Duration = Duration::from_secs(3);
// CONFIGRET value from cfgmgr32.h; keep this policy testable without Windows.
const CR_REMOVE_VETOED: u32 = 0x17;

/// Retries only transient PnP vetoes. The caller must validate the complete
/// device impact before entering, then revalidate target identity/topology in
/// `validate_target` before every request. No previous target handle is reused.
/// `request` reports the synchronous API's actual duration. Slow refusals are
/// returned without repeating that wait. The budget counts completed requests
/// and retry delays; it only decides whether another request may start and does
/// not interrupt, time out, or cancel an in-flight Windows API call.
pub fn request_with_retry<T, V, R, W>(
    cancellation: &Cancellation,
    mut validate_target: V,
    mut request: R,
    mut wait: W,
) -> Result<EjectResult, BackendError>
where
    V: FnMut() -> Result<T, BackendError>,
    R: FnMut(T) -> Result<(EjectResult, Duration), BackendError>,
    W: FnMut(Duration),
{
    let mut attempts = 0;
    let mut elapsed = Duration::ZERO;
    loop {
        check_cancellation(cancellation)?;
        let target = validate_target()?;
        check_cancellation(cancellation)?;
        let (result, request_duration) = request(target)?;
        attempts += 1;
        elapsed = elapsed.saturating_add(request_duration);
        if !is_transient_veto(&result) {
            return Ok(result);
        }
        check_cancellation(cancellation)?;
        if attempts == MAX_ATTEMPTS
            || request_duration >= SLOW_REQUEST_THRESHOLD
            || elapsed.saturating_add(RETRY_DELAY) >= RETRY_TIME_BUDGET
        {
            return Ok(result);
        }
        wait(RETRY_DELAY);
        elapsed = elapsed.saturating_add(RETRY_DELAY);
    }
}

fn check_cancellation(cancellation: &Cancellation) -> Result<(), BackendError> {
    if cancellation.is_cancelled() {
        Err(BackendError("提交 PnP 请求前已取消安全弹出".into()))
    } else {
        Ok(())
    }
}

fn is_transient_veto(result: &EjectResult) -> bool {
    matches!(
        result,
        EjectResult::Rejected(failure)
            if failure.config_ret == CR_REMOVE_VETOED
                && matches!(
                    failure.veto.as_ref(),
                    Some((VetoKind::OutstandingOpen | VetoKind::PendingClose, _))
                )
    )
}
