;; POC workload (architecture spike, see src/workload.rs): the minimal
;; module the agent<->workload interface POC needs -- one host import
;; (embewi_gpio_set), two exports (init/tick), nothing else. No timer,
;; sleep, WASI, filesystem, network, or async inside the sandbox -- Embassy
;; stays entirely on the agent side, driving tick() from its own timer.
;;
;; Declares a 1-page (64 KiB, the WASM spec minimum a module can declare)
;; linear memory, exported but never actually touched by this trivial
;; logic -- deliberately, so the POC's own measurement plan can isolate
;; "cost of a module having a memory at all" (step 4) from "cost of the
;; Module/Store/Instance machinery" (step 3), even though *this specific*
;; workload doesn't need memory for its own logic (toggling one GPIO needs
;; no buffers). A real workload with string/struct handling would need
;; more than the minimum.
(module
  (import "env" "embewi_gpio_set" (func $gpio_set (param i32 i32)))

  (memory (export "memory") 1)

  (global $pin i32 (i32.const 8))
  (global $state (mut i32) (i32.const 0))

  (func (export "init") (result i32)
    (global.set $state (i32.const 0))
    (i32.const 0))

  (func (export "tick") (result i32)
    (global.set $state (i32.xor (global.get $state) (i32.const 1)))
    (call $gpio_set (global.get $pin) (global.get $state))
    (i32.const 0)))
