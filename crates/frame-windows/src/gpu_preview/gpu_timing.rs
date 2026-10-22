//! Four opt-in GPU query sets, exclusively owned by the presenter context.
use super::Counters;
use std::sync::atomic::Ordering;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_QUERY, D3D11_QUERY_DATA_TIMESTAMP_DISJOINT,
    D3D11_QUERY_DESC, D3D11_QUERY_TIMESTAMP, D3D11_QUERY_TIMESTAMP_DISJOINT, ID3D11Device,
    ID3D11DeviceContext, ID3D11Query,
};
use windows::core::Interface;

struct Slot {
    disjoint: ID3D11Query,
    start: ID3D11Query,
    end: ID3D11Query,
    pending: bool,
}
pub(super) struct GpuClock {
    slots: Vec<Slot>,
}

fn query(device: &ID3D11Device, kind: D3D11_QUERY) -> Option<ID3D11Query> {
    let mut query = None;
    unsafe {
        device.CreateQuery(
            &D3D11_QUERY_DESC {
                Query: kind,
                MiscFlags: 0,
            },
            Some(&mut query),
        )
    }
    .ok()?;
    query
}
fn nanos(start: u64, end: u64, frequency: u64, disjoint: bool) -> Option<u64> {
    if disjoint || frequency == 0 {
        return None;
    }
    u64::try_from(u128::from(end.checked_sub(start)?) * 1_000_000_000 / u128::from(frequency)).ok()
}

// The generated Result discards S_FALSE (positive HRESULT). Preserve it: pending
// query memory is not data. No implicit flush or spin is allowed here.
fn data<T: Default>(context: &ID3D11DeviceContext, query: &ID3D11Query) -> Result<Option<T>, ()> {
    let mut value = T::default();
    let status = unsafe {
        (Interface::vtable(context).GetData)(
            context.as_raw(),
            query.as_raw(),
            (&mut value as *mut T).cast(),
            std::mem::size_of::<T>() as u32,
            D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
        )
    };
    match status.0 {
        0 => Ok(Some(value)),
        1 => Ok(None),
        _ => Err(()),
    }
}

impl GpuClock {
    pub(super) fn new(context: &ID3D11DeviceContext) -> Option<Self> {
        let device = unsafe { context.GetDevice() }.ok()?;
        let slots = (0..4)
            .map(|_| {
                Some(Slot {
                    disjoint: query(&device, D3D11_QUERY_TIMESTAMP_DISJOINT)?,
                    start: query(&device, D3D11_QUERY_TIMESTAMP)?,
                    end: query(&device, D3D11_QUERY_TIMESTAMP)?,
                    pending: false,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self { slots })
    }
    pub(super) fn collect(&mut self, context: &ID3D11DeviceContext, counters: &Counters) {
        for slot in &mut self.slots {
            if !slot.pending {
                continue;
            }
            let result: Result<Option<Option<u64>>, ()> = (|| {
                let Some(disjoint) =
                    data::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>(context, &slot.disjoint)?
                else {
                    return Ok(None);
                };
                let Some(start) = data::<u64>(context, &slot.start)? else {
                    return Ok(None);
                };
                let Some(end) = data::<u64>(context, &slot.end)? else {
                    return Ok(None);
                };
                Ok(Some(nanos(
                    start,
                    end,
                    disjoint.Frequency,
                    disjoint.Disjoint.as_bool(),
                )))
            })();
            match result {
                Ok(None) => continue,
                Ok(Some(Some(nanos))) => counters.presenter_gpu_span.record(nanos),
                _ => {
                    counters.gpu_timing_invalid.fetch_add(1, Ordering::Relaxed);
                }
            }
            slot.pending = false;
        }
    }
    pub(super) fn begin(
        &mut self,
        context: &ID3D11DeviceContext,
        counters: &Counters,
    ) -> Option<usize> {
        let Some(index) = self.slots.iter().position(|slot| !slot.pending) else {
            counters.gpu_timing_busy.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        unsafe {
            context.Begin(&self.slots[index].disjoint);
            context.End(&self.slots[index].start);
        }
        Some(index)
    }
    pub(super) fn end(&mut self, context: &ID3D11DeviceContext, index: usize) {
        let slot = &mut self.slots[index];
        unsafe {
            context.End(&slot.end);
            context.End(&slot.disjoint);
        }
        slot.pending = true;
    }
    pub(super) fn pending(&self) -> u64 {
        self.slots.iter().filter(|slot| slot.pending).count() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disjoint_zero_frequency_regression_and_overflow_never_become_gpu_samples() {
        assert_eq!(nanos(100, 400, 1_000_000, false), Some(300_000));
        assert_eq!(nanos(100, 400, 1_000_000, true), None);
        assert_eq!(nanos(100, 400, 0, false), None);
        assert_eq!(nanos(400, 100, 1_000_000, false), None);
        assert_eq!(nanos(0, u64::MAX, 1, false), None);
    }
}
