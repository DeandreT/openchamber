//! CSS Color 4 conversion matrices, D50/D65 chromatic adaptation and transfer
//! functions. Intermediate channels remain extended for color interpolation.

use super::PaintError;

type Triple = [f64; 3];
type Matrix = [Triple; 3];

const SRGB: Matrix = [
    [0.4123907992659595, 0.35758433938387796, 0.1804807884018343],
    [0.21263900587151036, 0.7151686787677559, 0.07219231536073371],
    [0.01933081871559185, 0.11919477979462599, 0.9505321522496607],
];
const P3: Matrix = [
    [0.4865709486482162, 0.26566769316909306, 0.1982172852343625],
    [0.2289745640697488, 0.6917385218365064, 0.079286914093745],
    [0.0, 0.04511338185890264, 1.043944368900976],
];
const A98: Matrix = [
    [0.5766690429101305, 0.1855582379065463, 0.1882286462349947],
    [0.29734497525053605, 0.6273635662554661, 0.07529145849399788],
    [0.02703136138641234, 0.07068885253582723, 0.9913375368376388],
];
const PROPHOTO: Matrix = [
    [0.7977666449006423, 0.13518129740053308, 0.0313477341283922],
    [0.2880748288194013, 0.711835234241873, 0.00008993693872564],
    [0.0, 0.0, 0.8251046025104601],
];
const REC2020: Matrix = [
    [0.6369580483012914, 0.14461690358620832, 0.1688809751641721],
    [0.2627002120112671, 0.6779980715188708, 0.05930171646986196],
    [0.0, 0.028072693049087428, 1.060985057710791],
];
const D65_TO_D50: Matrix = [
    [
        1.0479297925449969,
        0.022946870601609652,
        -0.05019226628920524,
    ],
    [
        0.02962780877005599,
        0.9904344267538799,
        -0.017073799063418826,
    ],
    [
        -0.009243040646204504,
        0.015055191490298152,
        0.7518742814281371,
    ],
];
// Match Chromium's gfx::LabToXYZD50 / XYZD50ToLab white and Skia's
// SkNamedGamut::kSRGB ICC 16.16 D50 matrix. Tiny neutral-axis differences
// become visible hue differences when Lab/LCH endpoints enter HSL mixes.
const LAB_WHITE: Triple = [0.9642, 1.0, 0.8251];
const SRGB_D50: Matrix = [
    [
        0x6FA2 as f64 / 65536.0,
        0x6299 as f64 / 65536.0,
        0x24A0 as f64 / 65536.0,
    ],
    [
        0x38F5 as f64 / 65536.0,
        0xB785 as f64 / 65536.0,
        0x0F84 as f64 / 65536.0,
    ],
    [
        0x0390 as f64 / 65536.0,
        0x18DA as f64 / 65536.0,
        0xB6CF as f64 / 65536.0,
    ],
];

fn multiply(matrix: Matrix, value: Triple) -> Triple {
    matrix.map(|row| row[0] * value[0] + row[1] * value[1] + row[2] * value[2])
}

fn inverse(m: Matrix) -> Matrix {
    let [[a, b, c], [d, e, f], [g, h, i]] = m;
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    [
        [e * i - f * h, c * h - b * i, b * f - c * e],
        [f * g - d * i, a * i - c * g, c * d - a * f],
        [d * h - e * g, b * g - a * h, a * e - b * d],
    ]
    .map(|row| row.map(|v| v / det))
}

fn signed_pow(value: f64, power: f64) -> f64 {
    value.signum() * value.abs().powf(power)
}

fn linear(v: f64) -> f64 {
    if v.abs() <= 0.04045 {
        v / 12.92
    } else {
        v.signum() * ((v.abs() + 0.055) / 1.055).powf(2.4)
    }
}

fn gamma(v: f64) -> f64 {
    if v.abs() <= 0.0031308 {
        v * 12.92
    } else {
        v.signum() * (1.055 * v.abs().powf(1.0 / 2.4) - 0.055)
    }
}

fn decode(space: &str, v: f64) -> f64 {
    match space {
        "a98-rgb" => signed_pow(v, 563.0 / 256.0),
        "prophoto-rgb" => {
            if v.abs() <= 16.0 / 512.0 {
                v / 16.0
            } else {
                signed_pow(v, 1.8)
            }
        }
        "rec2020" => {
            if v.abs() < 0.018053968510807 * 4.5 {
                v / 4.5
            } else {
                v.signum()
                    * ((v.abs() + 1.09929682680944 - 1.0) / 1.09929682680944).powf(1.0 / 0.45)
            }
        }
        _ => linear(v),
    }
}

fn encode(space: &str, v: f64) -> f64 {
    match space {
        "a98-rgb" => signed_pow(v, 256.0 / 563.0),
        "prophoto-rgb" => {
            if v.abs() <= 1.0 / 512.0 {
                v * 16.0
            } else {
                signed_pow(v, 1.0 / 1.8)
            }
        }
        "rec2020" => {
            if v.abs() < 0.018053968510807 {
                v * 4.5
            } else {
                v.signum() * (1.09929682680944 * v.abs().powf(0.45) - (1.09929682680944 - 1.0))
            }
        }
        _ => gamma(v),
    }
}

fn rgb_matrix(space: &str) -> Result<Matrix, PaintError> {
    match space {
        "srgb" => Ok(SRGB),
        "display-p3" => Ok(P3),
        "a98-rgb" => Ok(A98),
        "prophoto-rgb" => Ok(PROPHOTO),
        "rec2020" => Ok(REC2020),
        _ => Err(PaintError::Unsupported("color space")),
    }
}

fn cartesian(value: Triple) -> Triple {
    [
        value[0],
        value[1] * value[2].to_radians().cos(),
        value[1] * value[2].to_radians().sin(),
    ]
}

fn polar(value: Triple) -> Triple {
    [
        value[0],
        value[1].hypot(value[2]),
        value[2].atan2(value[1]).to_degrees().rem_euclid(360.0),
    ]
}

fn to_oklab(rgb: Triple) -> Triple {
    let [r, g, b] = rgb.map(linear);
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    [
        0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
    ]
}

fn from_oklab(lab: Triple) -> Triple {
    let l = (lab[0] + 0.3963377774 * lab[1] + 0.2158037573 * lab[2]).powi(3);
    let m = (lab[0] - 0.1055613458 * lab[1] - 0.0638541728 * lab[2]).powi(3);
    let s = (lab[0] - 0.0894841775 * lab[1] - 1.291485548 * lab[2]).powi(3);
    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
    ]
    .map(gamma)
}

fn from_lab(lab: Triple) -> Triple {
    let y = (lab[0] + 16.0) / 116.0;
    let values = [lab[1] / 500.0 + y, y, y - lab[2] / 200.0];
    let xyz: Triple = std::array::from_fn(|index| {
        let f = values[index];
        let cube = f.powi(3);
        (if cube > 216.0 / 24389.0 {
            cube
        } else {
            (116.0 * f - 16.0) / (24389.0 / 27.0)
        }) * LAB_WHITE[index]
    });
    multiply(inverse(SRGB_D50), xyz).map(gamma)
}

fn to_lab(rgb: Triple) -> Triple {
    let xyz = multiply(SRGB_D50, rgb.map(linear));
    let f: Triple = std::array::from_fn(|index| {
        let value = xyz[index] / LAB_WHITE[index];
        if value > 216.0 / 24389.0 {
            value.cbrt()
        } else {
            ((24389.0 / 27.0) * value + 16.0) / 116.0
        }
    });
    [
        116.0 * f[1] - 16.0,
        500.0 * (f[0] - f[1]),
        200.0 * (f[1] - f[2]),
    ]
}

fn from_hsl([h, s, l]: Triple) -> Triple {
    let h = h.rem_euclid(360.0) / 30.0;
    let a = s * l.min(1.0 - l);
    [0.0, 8.0, 4.0].map(|offset| {
        let k = (offset + h) % 12.0;
        l - a * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0)
    })
}

fn to_hsl([r, g, b]: Triple) -> Triple {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let l = (max + min) / 2.0;
    if d.abs() < 1e-12 {
        return [0.0, 0.0, l];
    }
    let h = if max == r {
        (g - b) / d
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    let s = if l == 0.0 || l == 1.0 {
        0.0
    } else {
        (max - l) / l.min(1.0 - l)
    };
    [h.rem_euclid(6.0) * 60.0, s, l]
}

pub(super) fn from_space(space: &str, value: Triple) -> Result<Triple, PaintError> {
    Ok(match space {
        "srgb" => value,
        "srgb-linear" => value.map(gamma),
        "oklab" => from_oklab(value),
        "oklch" => from_oklab(cartesian(value)),
        "lab" => from_lab(value),
        "lch" => from_lab(cartesian(value)),
        "hsl" => from_hsl(value),
        "hwb" => {
            let [h, w, b] = value;
            if w + b >= 1.0 {
                [w / (w + b); 3]
            } else {
                from_hsl([h, 1.0, 0.5]).map(|c| c * (1.0 - w - b) + w)
            }
        }
        "xyz" | "xyz-d65" => multiply(inverse(SRGB), value).map(gamma),
        "xyz-d50" => multiply(inverse(SRGB), multiply(inverse(D65_TO_D50), value)).map(gamma),
        _ => {
            let xyz = multiply(rgb_matrix(space)?, value.map(|v| decode(space, v)));
            let xyz = if space == "prophoto-rgb" {
                multiply(inverse(D65_TO_D50), xyz)
            } else {
                xyz
            };
            multiply(inverse(SRGB), xyz).map(gamma)
        }
    })
}

pub(super) fn to_space(space: &str, value: Triple) -> Result<Triple, PaintError> {
    Ok(match space {
        "srgb" => value,
        "srgb-linear" => value.map(linear),
        "oklab" => to_oklab(value),
        "oklch" => polar(to_oklab(value)),
        "lab" => to_lab(value),
        "lch" => polar(to_lab(value)),
        "hsl" => to_hsl(value),
        "hwb" => [
            to_hsl(value)[0],
            value[0].min(value[1]).min(value[2]),
            1.0 - value[0].max(value[1]).max(value[2]),
        ],
        "xyz" | "xyz-d65" => multiply(SRGB, value.map(linear)),
        "xyz-d50" => multiply(D65_TO_D50, multiply(SRGB, value.map(linear))),
        _ => {
            let xyz = multiply(SRGB, value.map(linear));
            let xyz = if space == "prophoto-rgb" {
                multiply(D65_TO_D50, xyz)
            } else {
                xyz
            };
            multiply(inverse(rgb_matrix(space)?), xyz).map(|v| encode(space, v))
        }
    })
}
