// Status codes returned by the C ABI (crates/nexa-client/src/abi.rs).
// Hand-written: csbindgen cannot emit negative constants.
namespace NexaDesk.Native
{
    public static unsafe partial class NativeMethods
    {
        public const int NEXA_E_INVALID_ARG = -1;
        public const int NEXA_E_BAD_HANDLE = -2;
        public const int NEXA_E_PARSE = -3;
        public const int NEXA_E_INTERNAL = -4;
        public const int NEXA_E_PANIC = -5;
        public const int NEXA_E_NOT_PERMITTED = -6;
        public const int NEXA_E_BUFFER_TOO_SMALL = -7;
        public const int NEXA_E_VIEW = -8;
    }
}
