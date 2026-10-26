#pragma once
#include <stdbool.h>
// Key-window identity is application-relative, not proof the application is
// active. First responder is additionally required for dispatch; beginning an
// activation makes the image view first responder itself.
static inline bool VNInputMayBegin(bool applicationActive,bool windowKey){
    return applicationActive&&windowKey;
}
static inline bool VNInputMayDispatch(bool applicationActive,bool windowKey,bool imageFirstResponder){
    return VNInputMayBegin(applicationActive,windowKey)&&imageFirstResponder;
}
