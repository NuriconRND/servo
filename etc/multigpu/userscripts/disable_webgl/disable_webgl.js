(function () {
  function denyWebGLContext(type) {
    return typeof type === "string" && (
      type.toLowerCase() === "webgl" ||
      type.toLowerCase() === "experimental-webgl" ||
      type.toLowerCase() === "webgl2" ||
      type.toLowerCase() === "experimental-webgl2"
    );
  }

  function patchGetContext(proto) {
    if (!proto || !proto.getContext || proto.__servoWallWebGLDisabled) {
      return;
    }

    var originalGetContext = proto.getContext;
    Object.defineProperty(proto, "__servoWallWebGLDisabled", {
      value: true,
      configurable: false
    });

    proto.getContext = function (type) {
      if (denyWebGLContext(type)) {
        return null;
      }
      return originalGetContext.apply(this, arguments);
    };
  }

  patchGetContext(window.HTMLCanvasElement && window.HTMLCanvasElement.prototype);

  if (window.OffscreenCanvas) {
    patchGetContext(window.OffscreenCanvas.prototype);
  }
}());
