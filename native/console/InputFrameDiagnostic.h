#import <Foundation/Foundation.h>
// Optional observation only. Never consulted by frame or authority validation.
BOOL VNInputDiagnosticIDValid(id value);
@interface VNInputFrameDiagnostic : NSObject
- (instancetype)initWithEnabled:(BOOL)enabled;
@property(readonly) BOOL enabled;
- (NSNumber *)acquireToken:(NSDictionary *)token observation:(NSDictionary *)observation epoch:(uint64_t)epoch;
- (void)dispatched:(NSNumber *)identifier sequence:(uint64_t)sequence epoch:(uint64_t)epoch now:(double)now queueAge:(double)age;
- (void)validated:(NSNumber *)identifier token:(NSDictionary *)token observation:(NSDictionary *)observation accepted:(BOOL)accepted reason:(NSString *)reason;
- (NSDictionary *)statistics;
@end
