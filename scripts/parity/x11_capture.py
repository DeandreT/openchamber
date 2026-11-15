"""Capture an owned window on the parity runner's 24-bit Xvfb display."""

import ctypes
import ctypes.util
import os
from pathlib import Path

import png
import virtual_display


def capture(window: str, width: int, height: int, path: Path) -> None:
    virtual_display.verify_owned_display()
    if width <= 0 or height <= 0 or width * height > 100_000_000:
        raise ValueError("invalid X11 capture dimensions")
    library = ctypes.util.find_library("X11")
    if library is None:
        raise RuntimeError("libX11 is unavailable")
    xlib = ctypes.CDLL(library)
    xlib.XOpenDisplay.argtypes = [ctypes.c_char_p]
    xlib.XOpenDisplay.restype = ctypes.c_void_p
    xlib.XCloseDisplay.argtypes = [ctypes.c_void_p]
    xlib.XGetImage.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                              ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_int]
    xlib.XGetImage.restype = ctypes.c_void_p
    xlib.XGetPixel.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
    xlib.XGetPixel.restype = ctypes.c_ulong
    xlib.XDestroyImage.argtypes = [ctypes.c_void_p]
    display = xlib.XOpenDisplay(os.environ["DISPLAY"].encode())
    if not display:
        raise RuntimeError("cannot connect to the owned Xvfb display")
    image = None
    try:
        image = xlib.XGetImage(display, int(window, 0), 0, 0, width, height, ctypes.c_ulong(-1).value, 2)
        if not image:
            raise RuntimeError("the owned X11 window could not be captured")
        rgba = bytearray(width * height * 4)
        for y in range(height):
            for x in range(width):
                pixel = xlib.XGetPixel(image, x, y)
                offset = (y * width + x) * 4
                rgba[offset:offset + 4] = bytes(((pixel >> 16) & 255, (pixel >> 8) & 255, pixel & 255, 255))
        png.write(path, png.Image(width, height, rgba))
    finally:
        if image:
            xlib.XDestroyImage(image)
        xlib.XCloseDisplay(display)
