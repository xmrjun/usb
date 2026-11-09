use std::collections::BTreeSet;

use crate::{
    backend::{BackendError, Cancellation},
    model::Volume,
};

/// Platform operations for preparing only the already-confirmed target volumes.
/// `verify` must identify the volume through the same handle that will be locked.
/// Implementations must own their handles and retain a Drop cleanup fallback.
pub trait VolumePreparation {
    type Handle;

    fn open(&mut self, volume: &Volume) -> Result<Self::Handle, BackendError>;
    fn verify(&mut self, handle: &Self::Handle, volume: &Volume) -> Result<(), BackendError>;
    fn lock(&mut self, handle: &Self::Handle, volume: &Volume) -> Result<(), BackendError>;
    fn flush(&mut self, handle: &Self::Handle, volume: &Volume) -> Result<(), BackendError>;
    fn dismount(&mut self, handle: &Self::Handle, volume: &Volume) -> Result<(), BackendError>;
    fn close(&mut self, handle: Self::Handle, volume: &Volume) -> Result<(), BackendError>;
    fn validate_target(&mut self) -> Result<(), BackendError>;
}

/// Locks every target volume before dismounting any of them. Success means all
/// preparation handles have been closed; it does not mean a device was ejected.
/// The caller must revalidate and submit the separate PnP removal request.
pub fn prepare_volumes<O: VolumePreparation>(
    volumes: &[Volume],
    cancellation: &Cancellation,
    ops: &mut O,
) -> Result<(), BackendError> {
    if volumes.is_empty() {
        return Err(BackendError("没有关联的逻辑卷，无法执行卷准备。".into()));
    }
    let mut unique_ids = BTreeSet::new();
    if volumes
        .iter()
        .any(|volume| !unique_ids.insert(volume.id.to_ascii_uppercase()))
    {
        return Err(BackendError("卷列表包含重复标识，已停止卷准备。".into()));
    }

    let mut opened = Vec::with_capacity(volumes.len());
    let mut dismounted = 0;
    let preparation = prepare_open_volumes(
        volumes,
        cancellation,
        ops,
        &mut opened,
        &mut dismounted,
    );

    // Cancellation must never skip cleanup. Continue closing other handles even
    // if an individual close fails, and preserve both the primary and close errors.
    let mut close_errors = Vec::new();
    for (index, handle) in opened.into_iter().rev() {
        if let Err(error) = ops.close(handle, &volumes[index]) {
            close_errors.push(format!("关闭卷 {} 失败：{error}", volumes[index].id));
        }
    }

    let result = preparation.and_then(|()| check_cancellation(cancellation));
    let mut problem = match result {
        Ok(()) if close_errors.is_empty() => return Ok(()),
        Ok(()) => close_errors.join("；"),
        Err(error) => {
            let mut message = error.0;
            if !close_errors.is_empty() {
                message.push(' ');
                message.push_str(&close_errors.join("；"));
            }
            message
        }
    };
    if dismounted > 0 {
        problem.push_str(&format!(
            " 已完成卸载 {dismounted} 个卷，尚未提交后续设备弹出请求；未执行卸载回滚。"
        ));
    }
    Err(BackendError(problem))
}

fn prepare_open_volumes<O: VolumePreparation>(
    volumes: &[Volume],
    cancellation: &Cancellation,
    ops: &mut O,
    opened: &mut Vec<(usize, O::Handle)>,
    dismounted: &mut usize,
) -> Result<(), BackendError> {
    check_cancellation(cancellation)?;
    ops.validate_target()?;
    for (index, volume) in volumes.iter().enumerate() {
        check_cancellation(cancellation)?;
        let handle = ops.open(volume)?;
        // Register ownership before verification so failure/cancellation there
        // cannot leak the newly opened handle.
        opened.push((index, handle));
        check_cancellation(cancellation)?;
        let (_, handle) = opened.last().expect("just pushed a volume handle");
        ops.verify(handle, volume)?;
    }
    for (index, handle) in opened.iter() {
        check_cancellation(cancellation)?;
        ops.lock(handle, &volumes[*index])?;
    }
    for (index, handle) in opened.iter() {
        check_cancellation(cancellation)?;
        ops.flush(handle, &volumes[*index])?;
    }
    check_cancellation(cancellation)?;
    ops.validate_target()?;
    for (index, handle) in opened.iter() {
        check_cancellation(cancellation)?;
        ops.dismount(handle, &volumes[*index])?;
        *dismounted += 1;
    }
    check_cancellation(cancellation)
}

fn check_cancellation(cancellation: &Cancellation) -> Result<(), BackendError> {
    if cancellation.is_cancelled() {
        Err(BackendError("卷准备已取消，尚未提交后续设备弹出请求。".into()))
    } else {
        Ok(())
    }
}
