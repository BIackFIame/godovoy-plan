#include <jni.h>
#include <limits.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

extern uint8_t gp_valid_hosted_address(const uint8_t *, size_t);
extern void *gp_open(const char *);
extern char *gp_open_error(void);
extern char *gp_request(void *, const char *);
extern void gp_string_free(char *);

static char *copy_string(JNIEnv *env, jbyteArray value) {
    if (!value) return NULL;
    jsize n = (*env)->GetArrayLength(env, value);
    if (n <= 0 || n > 1024 * 1024) return NULL;
    char *bytes = malloc((size_t)n + 1);
    if (!bytes) return NULL;
    (*env)->GetByteArrayRegion(env, value, 0, n, (jbyte *)bytes);
    if ((*env)->ExceptionCheck(env) || memchr(bytes, 0, (size_t)n)) {
        free(bytes);
        return NULL;
    }
    bytes[n] = 0;
    return bytes;
}

JNIEXPORT jlong JNICALL Java_local_godovoyplan_mobile_Core_open(JNIEnv *env, jclass cls, jbyteArray path) {
    (void)cls;
    char *bytes = copy_string(env, path);
    if (!bytes) return 0;
    void *result = gp_open(bytes);
    free(bytes);
    return (jlong)(intptr_t)result;
}

static jbyteArray response_bytes(JNIEnv *env, char *response) {
    if (!response) return NULL;
    size_t len = strlen(response);
    jbyteArray out = NULL;
    if (len <= INT_MAX) {
        out = (*env)->NewByteArray(env, (jsize)len);
        if (out && len) (*env)->SetByteArrayRegion(env, out, 0, (jsize)len, (const jbyte *)response);
    }
    gp_string_free(response);
    return out;
}

JNIEXPORT jbyteArray JNICALL Java_local_godovoyplan_mobile_Core_openError(JNIEnv *env, jclass cls) {
    (void)cls;
    return response_bytes(env, gp_open_error());
}

JNIEXPORT jbyteArray JNICALL Java_local_godovoyplan_mobile_Core_request(JNIEnv *env, jclass cls, jlong handle,
                                                                        jbyteArray command) {
    (void)cls;
    char *bytes = copy_string(env, command);
    if (!bytes || !handle) {
        free(bytes);
        return NULL;
    }
    char *response = gp_request((void *)(intptr_t)handle, bytes);
    free(bytes);
    return response_bytes(env, response);
}

JNIEXPORT jboolean JNICALL Java_local_godovoyplan_mobile_Core_validHostedAddress(JNIEnv *env, jclass cls, jbyteArray input) {
    (void)cls;
    if (!input) return JNI_FALSE;
    jsize length = (*env)->GetArrayLength(env, input);
    if (length <= 0 || length > 300) return JNI_FALSE;
    jbyte bytes[300];
    (*env)->GetByteArrayRegion(env, input, 0, length, bytes);
    if ((*env)->ExceptionCheck(env)) return JNI_FALSE;
    return gp_valid_hosted_address((const uint8_t *)bytes, (size_t)length) ? JNI_TRUE : JNI_FALSE;
}
