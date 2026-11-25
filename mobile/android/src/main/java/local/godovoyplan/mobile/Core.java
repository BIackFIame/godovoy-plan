package local.godovoyplan.mobile;

final class Core {
    static {
        System.loadLibrary("godovoy_mobile_core");
        System.loadLibrary("godovoy_android");
    }
    static native boolean validHostedAddress(byte[] address);
    static native long open(byte[] privatePath);
    static native byte[] openError();
    static native byte[] request(long handle, byte[] command);
    static native void close(long handle);
    private Core() {}
}
