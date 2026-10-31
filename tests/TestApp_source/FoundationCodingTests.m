/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
// Keyed-archive round trips for NSURLRequest / NSMutableURLRequest.
#ifdef DEFINE_ME_WHEN_BUILDING_ON_MACOS
#import <Foundation/Foundation.h>
#else
#import "system_headers.h"
@interface NSURL : NSObject
+ (id)URLWithString:(NSString *)string;
+ (id)fileURLWithPath:(NSString *)path;
- (NSString *)absoluteString;
- (NSString *)path;
- (BOOL)isFileURL;
@end
@interface NSURLRequest : NSObject
+ (id)requestWithURL:(NSURL *)url
         cachePolicy:(NSUInteger)policy
     timeoutInterval:(double)timeout;
- (NSURL *)URL;
- (NSURL *)mainDocumentURL;
- (NSUInteger)cachePolicy;
- (double)timeoutInterval;
- (NSString *)HTTPMethod;
- (NSData *)HTTPBody;
- (NSDictionary *)allHTTPHeaderFields;
- (BOOL)HTTPShouldHandleCookies;
- (BOOL)allowsCellularAccess;
@end
@interface NSMutableURLRequest : NSURLRequest
- (void)setHTTPMethod:(NSString *)method;
- (void)setHTTPBody:(NSData *)body;
- (void)setValue:(NSString *)value forHTTPHeaderField:(NSString *)field;
- (void)setMainDocumentURL:(NSURL *)url;
- (void)setHTTPShouldHandleCookies:(BOOL)should;
- (void)setAllowsCellularAccess:(BOOL)allows;
@end
@interface NSKeyedArchiver : NSObject
+ (NSData *)archivedDataWithRootObject:(id)root;
@end
@interface NSKeyedUnarchiver : NSObject
+ (id)unarchiveObjectWithData:(NSData *)data;
@end
#endif
#include <string.h>

static NSData *make_body(void) {
  static const char bytes[] = "a=1&b=2";
  return [NSData dataWithBytes:bytes length:sizeof(bytes) - 1];
}

// A mutable request keeps its class and every relevant field.
int test_NSMutableURLRequest_keyed_archive_round_trip(void) {
  NSAutoreleasePool *pool = [[NSAutoreleasePool alloc] init];
  NSMutableURLRequest *request = [NSMutableURLRequest
      requestWithURL:[NSURL URLWithString:@"http://example.com/path?q=1"]
         cachePolicy:1
     timeoutInterval:12.5];
  [request setHTTPMethod:@"POST"];
  [request setHTTPBody:make_body()];
  [request setValue:@"text/plain" forHTTPHeaderField:@"Content-Type"];
  [request setValue:@"abc" forHTTPHeaderField:@"X-Token"];
  [request setMainDocumentURL:[NSURL URLWithString:@"http://example.com/"]];
  [request setHTTPShouldHandleCookies:NO];
  [request setAllowsCellularAccess:NO];

  NSData *archive = [NSKeyedArchiver archivedDataWithRootObject:request];
  NSMutableURLRequest *copy = [NSKeyedUnarchiver unarchiveObjectWithData:archive];
  int result = 0;
  if (copy == nil) result = -1;
  else if (copy == request) result = -2;
  else if (![copy isKindOfClass:[NSMutableURLRequest class]]) result = -3;
  else if (![[[copy URL] absoluteString]
              isEqualToString:@"http://example.com/path?q=1"]) result = -4;
  else if ([copy cachePolicy] != 1) result = -5;
  else if ([copy timeoutInterval] != 12.5) result = -6;
  else if (![[copy HTTPMethod] isEqualToString:@"POST"]) result = -7;
  else if (![[copy HTTPBody] isEqualToData:make_body()]) result = -8;
  else if (![[[copy allHTTPHeaderFields] objectForKey:@"X-Token"]
              isEqualToString:@"abc"]) result = -9;
  else if (![[[copy allHTTPHeaderFields] objectForKey:@"Content-Type"]
              isEqualToString:@"text/plain"]) result = -10;
  else if (![[[copy mainDocumentURL] absoluteString]
              isEqualToString:@"http://example.com/"]) result = -11;
  else if ([copy HTTPShouldHandleCookies]) result = -12;
  else if ([copy allowsCellularAccess]) result = -13;
  if (!result) {
    // The decoded request must be genuinely mutable, and independent of
    // the original.
    [copy setValue:@"changed" forHTTPHeaderField:@"X-Token"];
    [copy setHTTPMethod:@"PUT"];
    if (![[[request allHTTPHeaderFields] objectForKey:@"X-Token"]
            isEqualToString:@"abc"] ||
        ![[request HTTPMethod] isEqualToString:@"POST"])
      result = -14;
  }
  [pool drain];
  return result;
}

// An immutable request round-trips as immutable, with defaults preserved.
int test_NSURLRequest_keyed_archive_round_trip(void) {
  NSAutoreleasePool *pool = [[NSAutoreleasePool alloc] init];
  NSURLRequest *request =
      [NSURLRequest requestWithURL:[NSURL URLWithString:@"https://example.org/"]
                       cachePolicy:3
                   timeoutInterval:30.0];
  NSData *archive = [NSKeyedArchiver archivedDataWithRootObject:request];
  NSURLRequest *copy = [NSKeyedUnarchiver unarchiveObjectWithData:archive];
  int result = 0;
  if (copy == nil) result = -1;
  else if ([copy isKindOfClass:[NSMutableURLRequest class]]) result = -2;
  else if (![copy isKindOfClass:[NSURLRequest class]]) result = -3;
  else if (![[[copy URL] absoluteString]
              isEqualToString:@"https://example.org/"]) result = -4;
  else if ([copy cachePolicy] != 3) result = -5;
  else if ([copy timeoutInterval] != 30.0) result = -6;
  else if (![[copy HTTPMethod] isEqualToString:@"GET"]) result = -7;
  else if ([copy HTTPBody] != nil) result = -8;
  else if ([[copy allHTTPHeaderFields] count] != 0) result = -9;
  else if (![copy HTTPShouldHandleCookies]) result = -10;
  else if (![copy allowsCellularAccess]) result = -11;
  [pool drain];
  return result;
}

// File URLs must come back as file URLs, not generic ones.
int test_NSURLRequest_keyed_archive_file_url(void) {
  NSAutoreleasePool *pool = [[NSAutoreleasePool alloc] init];
  NSURLRequest *request = [NSURLRequest
      requestWithURL:[NSURL fileURLWithPath:@"/tmp/hyperhle-request.txt"]
         cachePolicy:0
     timeoutInterval:60.0];
  NSData *archive = [NSKeyedArchiver archivedDataWithRootObject:request];
  NSURLRequest *copy = [NSKeyedUnarchiver unarchiveObjectWithData:archive];
  int result = 0;
  if (copy == nil) result = -1;
  else if (![[copy URL] isFileURL]) result = -2;
  else if (![[[copy URL] path] isEqualToString:@"/tmp/hyperhle-request.txt"])
    result = -3;
  [pool drain];
  return result;
}

// The decoded request owns what it decoded: it must stay valid after the
// archive, the unarchiver and the autorelease pool that held the
// intermediates are gone.
int test_NSURLRequest_keyed_archive_ownership(void) {
  NSAutoreleasePool *outer = [[NSAutoreleasePool alloc] init];
  NSURLRequest *kept = nil;
  {
    NSAutoreleasePool *inner = [[NSAutoreleasePool alloc] init];
    NSMutableURLRequest *request = [NSMutableURLRequest
        requestWithURL:[NSURL URLWithString:@"http://example.com/own"]
           cachePolicy:0
       timeoutInterval:5.0];
    [request setHTTPMethod:@"POST"];
    [request setHTTPBody:make_body()];
    [request setValue:@"v" forHTTPHeaderField:@"K"];
    NSData *archive = [NSKeyedArchiver archivedDataWithRootObject:request];
    kept = [[NSKeyedUnarchiver unarchiveObjectWithData:archive] retain];
    [inner drain];
  }
  int result = 0;
  if (![[[kept URL] absoluteString] isEqualToString:@"http://example.com/own"])
    result = -1;
  else if (![[kept HTTPMethod] isEqualToString:@"POST"]) result = -2;
  else if (![[kept HTTPBody] isEqualToData:make_body()]) result = -3;
  else if (![[[kept allHTTPHeaderFields] objectForKey:@"K"]
              isEqualToString:@"v"]) result = -4;
  [kept release];
  [outer drain];
  return result;
}
