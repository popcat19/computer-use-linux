// Purpose: Verify crop attachment planning, omission identity and real MCP PNG transport through Pi conversion.

import { readFileSync } from "node:fs";
import { inflateSync } from "node:zlib";
import { describe,it,expect } from "vitest";
import { convertMcpResult } from "../extension/mcp-output.ts";

const image=(bytes:number)=>({type:"image",mimeType:"image/png",data:Buffer.alloc(bytes).toString("base64")});
const jsonTexts=(content:ReturnType<typeof convertMcpResult>)=>content.filter(block=>block.type==="text").flatMap(block=>{try{return [JSON.parse(block.text)];}catch{return [];}});

describe("bounded crop attachments",()=>{
    it("keeps first and third native images, removes middle reference, and labels adjacent captions",()=>{
        const content=convertMcpResult({content:[{type:"text",text:JSON.stringify({ok:true,regions:[
            {label:"first",source_index:0,ok:true,image:{content_index:1}},
            {label:"middle",source_index:1,ok:true,image:{content_index:2}},
            {label:"third",source_index:2,ok:true,image:{content_index:3}},
        ]})},image(1.5*1024*1024),image(.75*1024*1024),image(.25*1024*1024)]});
        const metadata=jsonTexts(content).find(value=>value.regions);
        expect(metadata.regions[0].image).toMatchObject({native_image_index:0,original_mcp_content_index:1});
        expect(metadata.regions[1]).toMatchObject({ok:false,image_omitted:{label:"middle",source_index:1}});
        expect(metadata.regions[1].image).toBeUndefined();
        expect(metadata.regions[2].image).toMatchObject({native_image_index:1,original_mcp_content_index:3});
        expect(content.filter(block=>block.type==="image")).toHaveLength(2);
        for (const [index,block] of content.entries()) if(block.type==="image") {
            const caption=JSON.parse((content[index-1] as {text:string}).text);
            expect(caption.label).toBe(caption.native_image_index===0?"first":"third");
            expect(caption.attachment_id).toBe(metadata.regions[caption.native_image_index===0?0:2].image.attachment_id);
        }
        expect(JSON.stringify(jsonTexts(content))).not.toContain('"content_index":2');
        expect(jsonTexts(content).find(value=>value.images_omitted).images_omitted[0]).toMatchObject({label:"middle",source_index:1});
    });
    it("repairs legacy references while retaining geometry and request-unique repeated labels",()=>{
        const call=()=>convertMcpResult({content:[{type:"text",text:JSON.stringify({regions:[
            {label:"same",source_index:0,width:10,height:10,crop_rect:{x:2,y:3,width:5,height:5},data_url:`data:image/png;base64,${image(1.5*1024*1024).data}`},
            {label:"middle",source_index:1,width:10,height:10,data_url:`data:image/png;base64,${image(.75*1024*1024).data}`},
            {label:"last",source_index:2,width:10,height:10,data_url:`data:image/png;base64,${image(.25*1024*1024).data}`},
        ]})}]});
        const first=call(),second=call();const a=jsonTexts(first).find(value=>value.regions),b=jsonTexts(second).find(value=>value.regions);
        expect(a.regions[0]).toMatchObject({width:10,height:10,crop_rect:{x:2,y:3,width:5,height:5},image:{native_image_index:0}});
        expect(a.regions[1].image).toBeUndefined();expect(a.regions[1].image_omitted).toMatchObject({label:"middle",source_index:1});
        expect(a.regions[2].image.native_image_index).toBe(1);
        expect(a.regions[0].image.attachment_id).not.toBe(b.regions[0].image.attachment_id);
        expect(first.filter(block=>block.type==="image")).toHaveLength(2);
        expect(first.filter(block=>block.type==="text").map(block=>block.text).join("\n")).not.toContain("base64");
    });
    it("feeds a successful real Rust MCP native-route PNG result through Pi conversion",()=>{
        const result=JSON.parse(readFileSync(new URL("./fixtures/zoom-native-result.json",import.meta.url),"utf8"));
        expect(result.isError).toBe(false);
        const content=convertMcpResult(result);
        const metadata=jsonTexts(content).find(value=>value.regions);
        expect(metadata.regions[0]).toMatchObject({label:"first",crop_rect:{x:2,y:2,width:4,height:2},transform:{output_to_capture_offset:[6,4],output_to_capture_scale:[.5,.5]},image:{native_image_index:0,original_mcp_content_index:1}});
        const block=content.find(block=>block.type==="image")!;
        if(block.type!=="image")throw new Error("missing native PNG");
        expect(firstRgbaPixel(Buffer.from(block.data,"base64"))).toEqual([6,4,0,255]);
        expect(content.filter(block=>block.type==="text").map(block=>block.text).join("\n")).not.toContain(block.data);
    });
    it("reserves adjacent crop bounds captions even when large metadata text is truncated",()=>{
        const content=convertMcpResult({content:[{type:"text",text:JSON.stringify({padding:"x".repeat(300_000),regions:[{label:"bounded",source_index:2,crop_rect:{x:3,y:4,width:5,height:6},width:10,height:12,factor:2,transform:{output_to_capture_offset:[100,200],output_to_capture_scale:[.5,.5]},image:{content_index:1}}]})},image(8)]});
        const index=content.findIndex(block=>block.type==="image");const caption=JSON.parse((content[index-1] as {text:string}).text);
        expect(caption).toMatchObject({label:"bounded",source_index:2,crop_rect:{x:3,y:4,width:5,height:6},width:10,height:12,factor:2,transform:{output_to_capture_offset:[100,200]}});
        expect(content.filter(block=>block.type==="text").reduce((sum,block)=>sum+block.text.length,0)).toBeLessThanOrEqual(200_000);
    });
    it("marks unavailable direct script handles rather than implying an attachment exists",()=>{
        const content=convertMcpResult({content:[{type:"text",text:JSON.stringify({$image:"old-script:0"})}]});
        expect(jsonTexts(content)[0]).toMatchObject({image_omitted:true,original_image_handle:"old-script:0"});
        expect(jsonTexts(content)[0].$image).toBeUndefined();
    });
    it("repairs content indices inside legacy MCP envelopes",()=>{
        const result=convertMcpResult({content:[{type:"text",text:JSON.stringify({capture:{content:[{type:"text",text:"caption"},image(8)],structuredContent:{regions:[{label:"legacy",source_index:3,image:{content_index:1}}]}}})}]});
        const metadata=jsonTexts(result).find(value=>value.capture);
        expect(metadata.capture.structuredContent.regions[0].image).toMatchObject({native_image_index:0});
        expect(result.filter(block=>block.type==="image")).toHaveLength(1);
    });
});

function firstRgbaPixel(png:Buffer):number[]{
    expect(png.subarray(0,8).toString("hex")).toBe("89504e470d0a1a0a");
    const chunks:Buffer[]=[];let width=0,height=0;
    for(let offset=8;offset<png.length;){const size=png.readUInt32BE(offset),type=png.toString("ascii",offset+4,offset+8),data=png.subarray(offset+8,offset+8+size);if(type==="IHDR"){width=data.readUInt32BE(0);height=data.readUInt32BE(4);expect(data[8]).toBe(8);expect(data[9]).toBe(6);}if(type==="IDAT")chunks.push(data);offset+=size+12;}
    expect([width,height]).toEqual([8,4]);
    const raw=inflateSync(Buffer.concat(chunks));const row=raw.subarray(1,1+width*4);const filter=raw[0];
    expect(filter).toBeLessThanOrEqual(4);
    // First pixel has zero previous-row and left neighbors for every PNG filter.
    return [...row.subarray(0,4)];
}
