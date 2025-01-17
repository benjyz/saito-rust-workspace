use saito_core::common::keep_time::KeepTime;

pub struct WasmTimeKeeper {}

impl KeepTime for WasmTimeKeeper {
    fn get_timestamp_in_ms(&self) -> u64 {
        let date = js_sys::Date::new_0();

        date.get_time() as u64
    }
}
