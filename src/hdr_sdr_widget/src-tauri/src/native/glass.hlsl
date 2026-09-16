// All coordinates are physical pixels. Shape deformation never scales desktop pixels.
cbuffer Scene : register(b0) {
    float4 viewport; // width, height, desktop-relative window origin x/y
    float4 capsule;  // center x/y, half width/height
    float4 desktop;  // texture width/height, linear FP16 flag, frame valid
    float4 material; // dip scale, fill fraction, SDR reference white, opacity
    float4 feedback; // hover, press, overscroll, HDR enabled
    float4 optics;   // displacement DIP, band DIP, dispersion DIP, blur DIP
    float4 pointer;  // pressure center in local physical pixels
    float4 timing;   // elapsed seconds, reveal scale
    float4 hdr;      // sharp target, broad target, display peak (scRGB)
};
Texture2D<float4> background : register(t0);
Texture2D<float4> adaptation : register(t1);
SamplerState linearSampler : register(s0);
struct Vertex { float4 position : SV_Position; };
Vertex vsMain(uint id : SV_VertexID) {
    Vertex o; float2 p = float2((id << 1) & 2, id & 2);
    o.position = float4(p * float2(2, -2) + float2(-1, 1), 0, 1); return o;
}
float3 linearize(float3 c) {
    return lerp(c / 12.92, pow(max((c + .055) / 1.055, 0), 2.4), step(.04045, c));
}
float3 pqToScRgb(float3 c) {
    float3 p=pow(max(c,0),1.0/78.84375);
    float3 nits=10000*pow(max(p-.8359375,0)/max(18.8515625-18.6875*p,.000001),1.0/.1593017578125);
    return mul(float3x3(1.660491,-.587641,-.072850,-.124550,1.132900,-.008349,-.018151,-.100579,1.118730),nits/80.0);
}
float3 sampleDesktop(float2 p) {
    if (desktop.w > 4.5) return material.z * .03;
    if (desktop.w > 3.5) return material.z * .85;
    if (desktop.w > 2.5) return material.z.xxx;
    // Deterministic acceptance card, enabled only by the explicit native test runner.
    if (desktop.w > 1.5) {
        float stripe = fmod(floor(p.x / 2), 2);
        float checker = fmod(floor(p.x / 8) + floor(p.y / 8), 2);
        return lerp(float3(.06, .12, .23), float3(.85, .88, .92), p.y % 160 < 80 ? stripe : checker) * material.z;
    }
    float2 uv = clamp(p, .5, desktop.xy - .5) / desktop.xy;
    float3 c = background.SampleLevel(linearSampler, uv, 0).rgb;
    return desktop.z>1.5 ? pqToScRgb(c) : desktop.z > .5 ? c : linearize(c) * material.z;
}
float4 psAdapt(Vertex input) : SV_Target {
    float luminance = 0;
    [unroll] for(int y=0;y<3;y++) {
        [unroll] for(int x=0;x<3;x++) {
            float2 p = capsule.xy + float2((x-1)*capsule.z*.55, capsule.w*(.2+y*.25));
            luminance += dot(sampleDesktop(viewport.zw+p),float3(.2126,.7152,.0722))/9;
        }
    }
    float target = smoothstep(.65,.95,luminance/max(material.z,.001));
    float previous = adaptation.Load(int3(0,0,0)).r;
    float tau = target>previous ? .08 : .18;
    return float4(lerp(previous,target,1-exp(-min(timing.x,.1)/tau)),0,0,1);
}
float3 frost(float2 p, float radius) {
    if (radius < .05) return sampleDesktop(p);
    // A single symmetric filter, constant in screen-space across every shape state.
    return sampleDesktop(p) * .5 + .125 * (
        sampleDesktop(p + float2(radius, 0)) + sampleDesktop(p - float2(radius, 0)) +
        sampleDesktop(p + float2(0, radius)) + sampleDesktop(p - float2(0, radius)));
}
float sdf(float2 p, float2 h) {
    float radius = min(h.x, h.y);
    float2 q = abs(p) - h + radius;
    return length(max(q, 0)) + min(max(q.x, q.y), 0) - radius;
}
float4 psMain(Vertex input) : SV_Target {
    float2 p = input.position.xy;
    float2 local = p - capsule.xy;
    float d = sdf(local, capsule.zw);
    float alpha = saturate(.5 - d) * material.w;
    if (alpha <= 0) return 0;
    float dip = material.x;
    float2 q = local - float2(0, clamp(local.y, -capsule.w + capsule.z, capsule.w - capsule.z));
    // Flat face with a rounded thick bevel; the broad shoulder is deliberately weak.
    // Clamp to the actual radius so opposite sides meet without a medial-axis seam.
    float2 normal = q / max(length(q), .0001);
    float band = min(optics.y * dip, capsule.z);
    float t = saturate(1 - max(-d, 0) / max(band, 1));
    float t2 = t * t;
    float t4 = t2 * t2;
    // Convex liquid shoulder: integrate the slope to obtain its surface height.
    // Both refraction normal and optical path now describe the SAME raised surface.
    // Zero first/second derivative at the flat face avoids a visible shoulder seam.
    // This is our tension-inspired profile, not an Apple-published/private formula.
    float t3 = t2 * t;
    float t6 = t4 * t2;
    float t7 = t6 * t;
    float heightScale = optics.x / 10.0;
    float slope = heightScale * (.24 * t2 + 3.8 * t6);
    float3 surfaceNormal = normalize(float3(normal * slope, 1));
    float3 ray = refract(float3(0, 0, -1), surfaceNormal, 1.0 / 1.5);
    // Trace to a planar back face with varying optical thickness.
    // This is an artistic bevel model, not Apple's private optical model.
    float thickness = optics.x * dip * .55 + heightScale * band * (
        .08 * (1 - t3) + (3.8 / 7.0) * (1 - t7));
    float2 bend = ray.xy / max(-ray.z, .1);
    float2 pos = viewport.zw + p + bend * thickness;
    // Rim lighting stays narrow even though the refractive shoulder is wider.
    float strength = 1 - smoothstep(0, 7.0 * dip, max(-d, 0));
    float blur = optics.w * dip;
    float3 color = frost(pos, blur);
    float2 ca = -bend * optics.z * dip;
    color.r = frost(pos - ca, blur).r;
    color.b = frost(pos + ca, blur).b;
    // A valid black desktop is black, never treated as a missing capture.
    if (desktop.w < .5) color = material.z * .035;
    float2 illumination = normalize(float2(-.51,-.86) + clamp((pointer.xy-capsule.xy)/max(capsule.w,1),-1,1)*.08*feedback.x);
    float light = saturate(dot(normal, illumination));
    float opposite = saturate(dot(normal, -illumination));
    float innerShade = exp(-pow((t-.68)/.16,2));
    float whiteAdapt = adaptation.Load(int3(0,0,0)).r;
    float vertical = saturate((p.y-capsule.y+capsule.w)/(2*capsule.w));
    float shade = .22*whiteAdapt*smoothstep(.55,1,vertical)*smoothstep(0,4*dip,-d);
    color *= 1-shade;
    float fillTop = capsule.y + capsule.w - 2 * capsule.w * saturate(material.y);
    float fillAlpha = smoothstep(fillTop - .5, fillTop + .5, p.y) * .34;
    fillAlpha *= smoothstep(0, 5 * dip, -d);
    float3 filled = sampleDesktop(pos) * (1-shade);
    color = lerp(color, lerp(filled, material.z.xxx, .18), fillAlpha / .34);
    // The liquid is an independently emissive HDR layer, rather than SDR white.
    // Spend only available display headroom; preserve brighter transmitted detail.
    float liquidHeadroom = feedback.w > .5 ? max(hdr.z - material.z, 0) : 0;
    float liquidTarget = material.z + .68 * liquidHeadroom;
    float liquidLuma = dot(color, float3(.2126,.7152,.0722));
    color += max(liquidTarget-liquidLuma,0) * (fillAlpha/.34) * .90 * step(.5,feedback.w);
    float meniscus = saturate(1 - abs(p.y - fillTop) / max(dip, 1));
    meniscus *= step(.002, material.y) * step(material.y, .998) * smoothstep(0, 3 * dip, -d);
    float darkLine = 1-smoothstep(.55*dip,1.2*dip,abs(p.y-fillTop-2*dip));
    darkLine *= step(.002,material.y)*step(material.y,.998)*smoothstep(0,3*dip,-d);
    color *= 1-(feedback.w > .5 ? .95 : .80)*whiteAdapt*darkLine;
    if (feedback.w > .5) {
        float surfaceTarget = material.z + .88 * liquidHeadroom;
        float surfaceLuma = dot(color,float3(.2126,.7152,.0722));
        color += max(surfaceTarget-surfaceLuma,0) * meniscus * .90;
    } else {
        color = lerp(color, material.z.xxx, meniscus * .5);
    }
    // Layer the solid glass after the liquid, so the fill cannot erase reflections.
    // Translucent contour shadow, composited above the fill and below reflections.
    // Soften only its mask: the transmitted desktop stays sharp. Confine it to
    // the glass interior so the shaped HWND keeps exact cross-app hit testing.
    float edgeDepth = max(-d, 0) / max(dip, .001);
    float contourShadow = exp(-edgeDepth / 2.4) * (1 - smoothstep(5, 8, edgeDepth));
    float shadowOpacity = (.12 + .08 * whiteAdapt) * (.75 + .25 * opposite);
    color *= 1 - contourShadow * shadowOpacity;
    // Narrow dark separation below a brighter, fine reflection contour.
    color *= 1-innerShade*(.025+.035*opposite);
    float darkRim = exp(-pow((-d/dip-1.2)/.50,2));
    color *= 1-darkRim*(.075+.12*opposite);
    float neighbour = dot(sampleDesktop(pos+normal*3*dip),float3(.2126,.7152,.0722))/max(material.z,.001);
    float ridge = exp(-pow((t-.86)/.035,2)) * pow(light,12);
    ridge *= .03+.97*saturate(neighbour);
    // A short reflection arc, localized by angle as well as distance.
    // Long straight sides have almost no contribution from this light.
    float sharp = exp(-pow((-d/dip-.55)/.30,2))*pow(light,24);
    float broadWeight = 1-exp(-1.8*ridge);
    float sharpWeight = 1-exp(-6*sharp*(1+.15*feedback.x+.1*feedback.y));
    float luminance = dot(color,float3(.2126,.7152,.0722));
    color += max(hdr.y-luminance,0)*broadWeight;
    luminance = dot(color,float3(.2126,.7152,.0722));
    color += max(hdr.x-luminance,0)*sharpWeight;
    float border = saturate(1 - abs(d + .55 * dip) / max(.6 * dip, 1));
    // Low-energy environment reflection outlines the sides without making a
    // continuous HDR neon rim. The concentrated specular arc retains HDR headroom.
    float contourLight = .08 + .11 * light + .03 * opposite;
    float contourLuma = dot(color,float3(.2126,.7152,.0722));
    color += min(max(hdr.z-contourLuma,0),material.z*contourLight) * border;
    float2 touch=(p-pointer.xy)/(18*dip);
    color += exp(-dot(touch,touch))*feedback.y*.055*material.z;
    if (feedback.w < .5) {
        float dotAlpha = saturate(.5 - (length(p - (capsule.xy + float2(0, -capsule.w + 12 * dip))) - 2.5 * dip));
        color = lerp(color, float3(1, .35, .01) * material.z, dotAlpha);
    }
    return float4(color * alpha, alpha);
}
