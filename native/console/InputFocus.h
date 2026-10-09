#pragma once
#include <stdbool.h>
// Key-window identity is application-relative, not proof the application is
// active. First responder is additionally required for dispatch, while explicit
// Take Control initially originates from its button rather than the image view.
static inline bool VNInputMayTakeControl(bool applicationActive,bool windowKey){
    return applicationActive&&windowKey;
}
static inline bool VNInputMayDispatch(bool applicationActive,bool windowKey,bool imageFirstResponder){
    return VNInputMayTakeControl(applicationActive,windowKey)&&imageFirstResponder;
}
