/* macOS system proxy transaction. This file deliberately never invokes a shell.
 * ABI status: 0 success; 1 argument; 2 authorization/preferences; 3 lock;
 * 4 no primary service; 5 unsupported/conflicting settings; 6 journal I/O;
 * 7 invalid journal; 8 configuration/commit; 9 apply notification.
 * The caller must serialize sessions and protect the journal's parent directory.
 * A journal is created exclusively before commit and retained on every error.
 * restore deletes it only after successful commit and apply notification.
 */
#include <CoreFoundation/CoreFoundation.h>
#include <SystemConfiguration/SystemConfiguration.h>
#include <Security/Authorization.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <limits.h>

#define MAX_JOURNAL (128 * 1024)
#define RELEASE(x) do { if (x) CFRelease(x); } while (0)
static CFStringRef const keys[2][3] = {
    { CFSTR("HTTPEnable"), CFSTR("HTTPProxy"), CFSTR("HTTPPort") },
    { CFSTR("HTTPSEnable"), CFSTR("HTTPSProxy"), CFSTR("HTTPSPort") }
};
static const void *get(CFDictionaryRef d, CFStringRef k) { return d ? CFDictionaryGetValue(d, k) : NULL; }
static int same(const void *a, const void *b) { return a == b || (a && b && CFEqual(a, b)); }
static int number(CFTypeRef x, int min, int max) {
    int n; return x && CFGetTypeID(x) == CFNumberGetTypeID() && !CFNumberIsFloatType(x) &&
        CFNumberGetValue(x, kCFNumberIntType, &n) && n >= min && n <= max;
}
static int valid_value(CFTypeRef x, int field) {
    if (field == 0) return number(x, 0, 1);
    if (field == 2) return number(x, 1, 65535);
    if (!x || CFGetTypeID(x) != CFStringGetTypeID()) return 0;
    CFIndex len = CFStringGetLength(x); if (!len || len > 253) return 0;
    for (CFIndex i = 0; i < len; i++) {
        UniChar c = CFStringGetCharacterAtIndex(x, i);
        if (c <= 32 || c >= 127 || c == '@' || c == '/' || c == '\\') return 0;
    }
    return 1;
}
static CFMutableDictionaryRef group(CFDictionaryRef d, int g) {
    CFMutableDictionaryRef r = CFDictionaryCreateMutable(NULL, 3, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    if (!r) return NULL;
    for (int i = 0; i < 3; i++) {
        CFTypeRef v = get(d, keys[g][i]);
        if (v) { if (!valid_value(v, i)) { CFRelease(r); return NULL; } CFDictionarySetValue(r, keys[g][i], v); }
    }
    return r;
}
static int valid_group(CFTypeRef d, int g) {
    if (!d || CFGetTypeID(d) != CFDictionaryGetTypeID()) return 0;
    CFIndex count = CFDictionaryGetCount(d), known = 0;
    for (int i = 0; i < 3; i++) { CFTypeRef v = get(d, keys[g][i]); if (v) { known++; if (!valid_value(v, i)) return 0; } }
    return count == known;
}
static void put_group(CFMutableDictionaryRef out, CFDictionaryRef values, int g) {
    for (int i = 0; i < 3; i++) { CFTypeRef v = get(values, keys[g][i]); if (v) CFDictionarySetValue(out, keys[g][i], v); else CFDictionaryRemoveValue(out, keys[g][i]); }
}
static CFMutableDictionaryRef installed(int g, unsigned short port) {
    CFMutableDictionaryRef d = CFDictionaryCreateMutable(NULL, 3, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    int one = 1, p = port; CFNumberRef a = CFNumberCreate(NULL, kCFNumberIntType, &one), b = CFNumberCreate(NULL, kCFNumberIntType, &p);
    if (!d || !a || !b) { RELEASE(d); RELEASE(a); RELEASE(b); return NULL; }
    CFDictionarySetValue(d, keys[g][0], a); CFDictionarySetValue(d, keys[g][1], CFSTR("127.0.0.1")); CFDictionarySetValue(d, keys[g][2], b);
    CFRelease(a); CFRelease(b); return d;
}
/* 0 already original, 1 ours, -1 changed by another actor. */
static int ownership(CFDictionaryRef current, CFDictionaryRef original, CFDictionaryRef owned, int g) {
    CFMutableDictionaryRef now = group(current, g);
    CFStringRef user = g ? CFSTR("HTTPSUser") : CFSTR("HTTPUser");
    CFStringRef password = g ? CFSTR("HTTPSPassword") : CFSTR("HTTPPassword");
    int state = now && CFEqual(now, original) ? 0 :
        now && CFEqual(now, owned) && !get(current, user) && !get(current, password) ? 1 : -1;
    RELEASE(now); return state;
}
/* Restore only groups still bearing our exact values; leave edits untouched. */
static int merge_restore(CFDictionaryRef current, CFMutableDictionaryRef next, CFDictionaryRef entry, unsigned short port) {
    int changed = 0;
    for (int g = 0; g < 2; g++) {
        CFDictionaryRef original = get(entry, g ? CFSTR("https") : CFSTR("http"));
        CFMutableDictionaryRef owned = installed(g, port); if (!owned) return -1;
        int state = ownership(current, original, owned, g); CFRelease(owned);
        if (state == 1) { put_group(next, original, g); changed = 1; }
    }
    return changed;
}
static int enabled(CFDictionaryRef d, CFStringRef k) {
    CFTypeRef v = get(d, k); if (!v) return 0;
    return !number(v, 0, 0); /* malformed values fail closed */
}
static int supported(CFDictionaryRef d) {
    /* Authentication can be held outside this dictionary; fail closed. */
    return !enabled(d, CFSTR("HTTPEnable")) && !enabled(d, CFSTR("HTTPSEnable")) &&
        !enabled(d, CFSTR("ProxyAutoConfigEnable")) && !enabled(d, CFSTR("ProxyAutoDiscoveryEnable")) &&
        !get(d, CFSTR("HTTPUser")) && !get(d, CFSTR("HTTPSUser")) &&
        !get(d, CFSTR("HTTPPassword")) && !get(d, CFSTR("HTTPSPassword"));
}
static int apply_retry(SCPreferencesRef p) {
    for (int i = 0; i < 3; i++) { if (SCPreferencesApplyChanges(p)) return 1; usleep(100000); } return 0;
}
/* Calls are serialized by the watchdog. Keep authorization for its lifetime so
 * normal restoration can reuse rights obtained while applying the proxy. */
static AuthorizationRef session_auth;
static SCPreferencesRef preferences(void) {
    if (!session_auth && AuthorizationCreate(NULL, kAuthorizationEmptyEnvironment, kAuthorizationFlagDefaults, &session_auth) != errAuthorizationSuccess) return NULL;
    return SCPreferencesCreateWithAuthorization(NULL, CFSTR("Exodys proxy"), NULL, session_auth);
}
static int sync_parent(const char *path) {
    char *s = strdup(path); if (!s) return 0; char *slash = strrchr(s, '/');
    const char *dir = "."; if (slash) { if (slash == s) slash[1] = 0; else *slash = 0; dir = s; }
    int fd = open(dir, O_RDONLY | O_DIRECTORY | O_CLOEXEC); int ok = fd >= 0 && fsync(fd) == 0; if (fd >= 0) close(fd); free(s); return ok;
}
static int write_journal(const char *path, CFPropertyListRef plist) {
    CFDataRef data = CFPropertyListCreateData(NULL, plist, kCFPropertyListBinaryFormat_v1_0, 0, NULL);
    if (!data || CFDataGetLength(data) > MAX_JOURNAL) { RELEASE(data); return 0; }
    size_t plen = strlen(path); if (plen > PATH_MAX - 32) { CFRelease(data); return 0; }
    char *tmp = malloc(plen + 32); if (!tmp) { CFRelease(data); return 0; }
    snprintf(tmp, plen + 32, "%s.tmp.XXXXXXXX", path);
    int fd = mkstemp(tmp); if (fd < 0) { free(tmp); CFRelease(data); return 0; }
    int ok = fchmod(fd, 0600) == 0;
    const UInt8 *p = CFDataGetBytePtr(data); size_t left = (size_t)CFDataGetLength(data);
    while (ok && left) { ssize_t n = write(fd, p, left); if (n < 0 && errno == EINTR) continue; if (n <= 0) { ok = 0; break; } p += n; left -= (size_t)n; }
    if (ok && fsync(fd)) ok = 0; if (close(fd)) ok = 0;
    if (ok && link(tmp, path)) ok = 0;
    if (unlink(tmp)) ok = 0;
    if (ok && !sync_parent(path)) ok = 0;
    free(tmp); CFRelease(data); return ok;
}
static CFPropertyListRef read_journal(const char *path) {
    int fd = open(path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC); if (fd < 0) return NULL;
    struct stat st; if (fstat(fd, &st) || !S_ISREG(st.st_mode) || st.st_uid != geteuid() || (st.st_mode & 077) || st.st_nlink != 1 || st.st_size <= 0 || st.st_size > MAX_JOURNAL) { close(fd); return NULL; }
    size_t len = (size_t)st.st_size, off = 0; UInt8 *buf = malloc(len); if (!buf) { close(fd); return NULL; }
    while (off < len) { ssize_t n = read(fd, buf + off, len - off); if (n < 0 && errno == EINTR) continue; if (n <= 0) break; off += (size_t)n; }
    close(fd); CFDataRef data = off == len ? CFDataCreate(NULL, buf, len) : NULL; free(buf);
    CFPropertyListRef result = data ? CFPropertyListCreateWithData(NULL, data, kCFPropertyListImmutable, NULL, NULL) : NULL; RELEASE(data); return result;
}
static int valid_id(CFTypeRef id) {
    if (!id || CFGetTypeID(id) != CFStringGetTypeID() || CFStringGetLength(id) != 36) return 0;
    CFUUIDRef u = CFUUIDCreateFromString(NULL, id); if (!u) return 0; CFStringRef canonical = CFUUIDCreateString(NULL, u);
    int ok = canonical && CFStringCompare(id, canonical, kCFCompareCaseInsensitive) == kCFCompareEqualTo; RELEASE(canonical); CFRelease(u); return ok;
}
static int valid_journal(CFTypeRef root) {
    if (!root || CFGetTypeID(root) != CFDictionaryGetTypeID() || CFDictionaryGetCount(root) != 2 || !number(get(root, CFSTR("port")), 1, 65535)) return 0;
    CFTypeRef a = get(root, CFSTR("services")); if (!a || CFGetTypeID(a) != CFArrayGetTypeID() || CFArrayGetCount(a) < 1 || CFArrayGetCount(a) > 2) return 0;
    for (CFIndex i = 0; i < CFArrayGetCount(a); i++) {
        CFTypeRef e = CFArrayGetValueAtIndex(a, i);
        if (!e || CFGetTypeID(e) != CFDictionaryGetTypeID() || CFDictionaryGetCount(e) != 3 || !valid_id(get(e, CFSTR("id"))) || !valid_group(get(e, CFSTR("http")), 0) || !valid_group(get(e, CFSTR("https")), 1)) return 0;
        if (i && CFEqual(get(e, CFSTR("id")), get(CFArrayGetValueAtIndex(a, 0), CFSTR("id")))) return 0;
    } return 1;
}
static SCNetworkServiceRef find_service(CFArrayRef services, CFStringRef id) {
    for (CFIndex i = 0; services && i < CFArrayGetCount(services); i++) { SCNetworkServiceRef s = CFArrayGetValueAtIndex(services, i); if (same(SCNetworkServiceGetServiceID(s), id)) return s; } return NULL;
}
static CFArrayRef primary_ids(void) {
    CFMutableArrayRef ids = CFArrayCreateMutable(NULL, 2, &kCFTypeArrayCallBacks); if (!ids) return NULL;
    SCDynamicStoreRef store = SCDynamicStoreCreate(NULL, CFSTR("Exodys proxy"), NULL, NULL);
    if (!store) { CFRelease(ids); return NULL; }
    CFStringRef states[] = { CFSTR("State:/Network/Global/IPv4"), CFSTR("State:/Network/Global/IPv6") };
    for (int i = 0; i < 2; i++) { CFTypeRef d = SCDynamicStoreCopyValue(store, states[i]); if (d && CFGetTypeID(d) == CFDictionaryGetTypeID()) { CFTypeRef id = get(d, CFSTR("PrimaryService")); if (valid_id(id) && !CFArrayContainsValue(ids, CFRangeMake(0, CFArrayGetCount(ids)), id)) CFArrayAppendValue(ids, id); } RELEASE(d); }
    CFRelease(store); return ids;
}
int exodys_proxy_apply(const char *path, unsigned short port) {
    if (!path || !*path || !port) return 1;
    int rc = 2, locked = 0; SCPreferencesRef p = preferences();
    SCNetworkSetRef set = NULL; CFArrayRef services = NULL, ids = NULL; CFMutableArrayRef entries = NULL; CFMutableDictionaryRef root = NULL;
    if (!p) goto done; rc = 3; if (!SCPreferencesLock(p, FALSE)) goto done; locked = 1;
    set = SCNetworkSetCopyCurrent(p); services = set ? SCNetworkSetCopyServices(set) : NULL; ids = primary_ids();
    rc = 4; if (!services || !ids || !CFArrayGetCount(ids)) goto done;
    entries = CFArrayCreateMutable(NULL, 0, &kCFTypeArrayCallBacks); root = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    rc = 8; if (!entries || !root) goto done;
    for (CFIndex i = 0; i < CFArrayGetCount(ids); i++) {
        CFStringRef id = CFArrayGetValueAtIndex(ids, i); SCNetworkServiceRef s = find_service(services, id);
        rc = 4; if (!s || !SCNetworkServiceGetEnabled(s)) goto done;
        SCNetworkProtocolRef proto = SCNetworkServiceCopyProtocol(s, kSCNetworkProtocolTypeProxies);
        rc = 8; if (!proto || !SCNetworkProtocolGetEnabled(proto)) { RELEASE(proto); goto done; }
        CFDictionaryRef current = SCNetworkProtocolGetConfiguration(proto);
        rc = 5; if (!supported(current)) { CFRelease(proto); goto done; }
        CFMutableDictionaryRef a = group(current, 0), b = group(current, 1);
        if (!a || !b) { RELEASE(a); RELEASE(b); CFRelease(proto); goto done; }
        CFMutableDictionaryRef entry = CFDictionaryCreateMutable(NULL, 3, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        CFMutableDictionaryRef next = current ? CFDictionaryCreateMutableCopy(NULL, 0, current) : CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        CFMutableDictionaryRef x = installed(0, port), y = installed(1, port); rc = 8;
        if (!entry || !next || !x || !y) { RELEASE(entry); RELEASE(next); RELEASE(x); RELEASE(y); CFRelease(a); CFRelease(b); CFRelease(proto); goto done; }
        CFDictionarySetValue(entry, CFSTR("id"), id); CFDictionarySetValue(entry, CFSTR("http"), a); CFDictionarySetValue(entry, CFSTR("https"), b); CFArrayAppendValue(entries, entry);
        put_group(next, x, 0); put_group(next, y, 1); Boolean ok = SCNetworkProtocolSetConfiguration(proto, next);
        CFRelease(entry); CFRelease(next); CFRelease(x); CFRelease(y); CFRelease(a); CFRelease(b); CFRelease(proto); if (!ok) goto done;
    }
    int pv = port; CFNumberRef pn = CFNumberCreate(NULL, kCFNumberIntType, &pv); if (!pn) goto done;
    CFDictionarySetValue(root, CFSTR("port"), pn); CFDictionarySetValue(root, CFSTR("services"), entries); CFRelease(pn);
    rc = 6; if (!write_journal(path, root)) goto done;
    rc = 8; if (!SCPreferencesCommitChanges(p)) goto done;
    rc = apply_retry(p) ? 0 : 9;
done:
    RELEASE(root); RELEASE(entries); RELEASE(ids); RELEASE(services); RELEASE(set); if (locked) SCPreferencesUnlock(p); RELEASE(p); return rc;
}
int exodys_proxy_restore(const char *path) {
    if (!path || !*path) return 1;
    struct stat st; if (lstat(path, &st) != 0) return errno == ENOENT ? 0 : 6;
    CFPropertyListRef root = read_journal(path); if (!root) return 6;
    if (!valid_journal(root)) { CFRelease(root); return 7; }
    int port = 0; CFNumberGetValue(get(root, CFSTR("port")), kCFNumberIntType, &port);
    int rc = 2, locked = 0, changed = 0; SCPreferencesRef p = preferences();
    CFArrayRef all = NULL; if (!p) goto done; rc = 3; if (!SCPreferencesLock(p, FALSE)) goto done; locked = 1;
    all = SCNetworkServiceCopyAll(p); rc = 8; if (!all) goto done;
    CFArrayRef entries = get(root, CFSTR("services"));
    for (CFIndex i = 0; i < CFArrayGetCount(entries); i++) {
        CFDictionaryRef entry = CFArrayGetValueAtIndex(entries, i); SCNetworkServiceRef s = find_service(all, get(entry, CFSTR("id")));
        if (!s) continue;
        SCNetworkProtocolRef proto = SCNetworkServiceCopyProtocol(s, kSCNetworkProtocolTypeProxies); if (!proto) continue;
        CFDictionaryRef current = SCNetworkProtocolGetConfiguration(proto); CFMutableDictionaryRef next = current ? CFDictionaryCreateMutableCopy(NULL, 0, current) : CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        if (!next) { CFRelease(proto); goto done; }
        int local = merge_restore(current, next, entry, (unsigned short)port);
        if (local < 0 || (local && !SCNetworkProtocolSetConfiguration(proto, next))) { CFRelease(next); CFRelease(proto); goto done; }
        changed |= local; CFRelease(next); CFRelease(proto);
    }
    rc = 8; if (changed && !SCPreferencesCommitChanges(p)) goto done;
    rc = 9; if (!apply_retry(p)) goto done;
    rc = 6; if (unlink(path) || !sync_parent(path)) goto done; rc = 0;
done:
    RELEASE(all); if (locked) SCPreferencesUnlock(p); RELEASE(p); CFRelease(root); return rc;
}
/* Pure dictionary test; does not open preferences or touch system settings. */
int exodys_proxy_test(void) {
    int result = 1; CFMutableDictionaryRef original = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    if (!original) return result; CFDictionarySetValue(original, CFSTR("ExceptionsList"), CFSTR("untouched"));
    CFMutableDictionaryRef a = group(original, 0), b = group(original, 1), x = installed(0, 4567), y = installed(1, 4567);
    CFMutableDictionaryRef current = CFDictionaryCreateMutableCopy(NULL, 0, original);
    CFMutableDictionaryRef entry = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    if (a && b && x && y && current && entry) {
        CFDictionarySetValue(entry, CFSTR("http"), a); CFDictionarySetValue(entry, CFSTR("https"), b);
        put_group(current, x, 0); put_group(current, y, 1);
        CFDictionarySetValue(current, CFSTR("HTTPProxy"), CFSTR("edited.example"));
        CFMutableDictionaryRef next = CFDictionaryCreateMutableCopy(NULL, 0, current);
        if (next && merge_restore(current, next, entry, 4567) == 1 &&
            same(get(next, CFSTR("HTTPProxy")), CFSTR("edited.example")) &&
            !get(next, CFSTR("HTTPSEnable")) && !get(next, CFSTR("HTTPSProxy")) && !get(next, CFSTR("HTTPSPort")) &&
            same(get(next, CFSTR("ExceptionsList")), CFSTR("untouched"))) result = 0;
        RELEASE(next);
    }
    RELEASE(a); RELEASE(b); RELEASE(x); RELEASE(y); RELEASE(current); RELEASE(entry); CFRelease(original); return result;
}
