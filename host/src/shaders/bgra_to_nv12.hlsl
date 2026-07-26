// Compute shader converting BGRA (capture output) to NV12 (Y plane + UV
// plane), entirely on the GPU. Uses BT.601 limited-range coefficients.

Texture2D<float4> InputTexture : register(t0);
RWTexture2D<float> OutputY : register(u0);
RWTexture2D<float2> OutputUV : register(u1);

[numthreads(8, 8, 1)]
void CSMain(uint3 DTid : SV_DispatchThreadID)
{
    uint2 pos = DTid.xy;
    uint width, height;
    InputTexture.GetDimensions(width, height);
    if (pos.x >= width || pos.y >= height)
    {
        return;
    }

    float3 rgb = InputTexture[pos].rgb;
    float y = dot(rgb, float3(0.257, 0.504, 0.098)) + 0.0625;
    OutputY[pos] = y;

    // Once per 2x2 block, subsample chroma by averaging up to 4 pixels.
    if ((pos.x % 2 == 0) && (pos.y % 2 == 0))
    {
        float3 sum = rgb;
        uint samples = 1;
        if (pos.x + 1 < width)
        {
            sum += InputTexture[pos + uint2(1, 0)].rgb;
            samples++;
        }
        if (pos.y + 1 < height)
        {
            sum += InputTexture[pos + uint2(0, 1)].rgb;
            samples++;
        }
        if (pos.x + 1 < width && pos.y + 1 < height)
        {
            sum += InputTexture[pos + uint2(1, 1)].rgb;
            samples++;
        }
        float3 avg = sum / samples;
        float u = dot(avg, float3(-0.148, -0.291, 0.439)) + 0.5;
        float v = dot(avg, float3(0.439, -0.368, -0.071)) + 0.5;
        OutputUV[pos / 2] = float2(u, v);
    }
}
