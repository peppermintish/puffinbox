(module
  ;; Original demonstration hook. It has no imports, networking, filesystem,
  ;; clocks, or other host capabilities. The output is intentionally fixed so
  ;; the plugin API can be exercised without third-party metadata or data.
  (memory (export "memory") 1 16)
  (data (i32.const 0) "{\"overview\":\"Applied the original example metadata hook.\"}")
  (func (export "enrich")
    (param $input_ptr i32)
    (param $input_len i32)
    (param $output_ptr i32)
    (param $output_capacity i32)
    (result i32)
    local.get $output_capacity
    i32.const 58
    i32.lt_u
    if (result i32)
      i32.const -1
    else
      local.get $output_ptr
      i32.const 0
      i32.const 58
      memory.copy
      i32.const 58
    end)
)
