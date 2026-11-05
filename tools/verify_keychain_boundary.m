// Probe only Intercat's keys; never print or keep their values.
#import <Foundation/Foundation.h>
#import <LocalAuthentication/LocalAuthentication.h>
#import <Security/Security.h>

int main(int argc, char **argv) {
  @autoreleasepool {
    if (argc != 2 ||
        (strcmp(argv[1], "rpc.ui") != 0 && strcmp(argv[1], "secrets.hmac.v1") != 0)) {
      fputs("usage: verify_keychain_boundary rpc.ui|secrets.hmac.v1\n", stderr);
      return 2;
    }
    LAContext *context = [LAContext new];
    context.interactionNotAllowed = YES;
    // Login-keychain ACL prompts do not consistently honor LAContext alone.
    // Disable interaction in both Security query APIs as well.
    SecKeychainSetUserInteractionAllowed(false);
    NSDictionary *query = @{
      (__bridge id)kSecClass: (__bridge id)kSecClassGenericPassword,
      (__bridge id)kSecAttrService: @"Intercat",
      (__bridge id)kSecAttrAccount: [NSString stringWithUTF8String:argv[1]],
      (__bridge id)kSecReturnData: @YES,
      (__bridge id)kSecMatchLimit: (__bridge id)kSecMatchLimitOne,
      (__bridge id)kSecUseAuthenticationContext: context
      ,(__bridge id)kSecUseAuthenticationUI: (__bridge id)kSecUseAuthenticationUIFail
    };
    CFTypeRef value = NULL;
    OSStatus status = SecItemCopyMatching((__bridge CFDictionaryRef)query, &value);
    if (value) CFRelease(value);
    printf("{\"status\":%d,\"readSucceeded\":%s,\"interactionDisabled\":%s}\n", (int)status,
           status == errSecSuccess ? "true" : "false", "true");
    return status == errSecSuccess ? 1 : 0;
  }
}
