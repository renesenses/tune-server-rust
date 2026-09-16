#include <cstdio>
#include <cstdlib>
#include <vector>
#include "All.h"
#include "APECompressCore.h"
#include "BitArray.h"
class Memory : public CIO {
public:
    std::vector<unsigned char> bytes;
    int Open(const char*) override { return 1; }
    int Close() override { return 0; }
    int Read(void*, unsigned, unsigned*) override { return 1; }
    int Write(const void* p, unsigned n, unsigned* written) override {
        auto b=static_cast<const unsigned char*>(p);
        bytes.insert(bytes.end(),b,b+n);*written=n;return 0;
    }
    int Seek(int,unsigned) override { return 1; }
    int Create(const char*) override { return 1; }
    int Delete() override { return 1; }
    int SetEOF() override { return 1; }
    int GetPosition() override { return bytes.size(); }
    int GetSize() override { return bytes.size(); }
    int GetName(char*) override { return 1; }
};
int IsAltiVecAvailable() { return 0; }
int main(int argc,char** argv) {
    if(argc!=6) return 2;
    int channels=std::atoi(argv[3]),bits=std::atoi(argv[4]),level=std::atoi(argv[5]);
    FILE* input=std::fopen(argv[1],"rb"); if(!input) return 3;
    std::fseek(input,0,SEEK_END);int size=std::ftell(input);std::rewind(input);
    std::vector<unsigned char> pcm(size);std::fread(pcm.data(),1,size,input);std::fclose(input);
    WAVEFORMATEX format={};format.wFormatTag=1;format.nChannels=channels;
    format.nSamplesPerSec=44100;format.wBitsPerSample=bits;
    format.nBlockAlign=channels*(bits/8);format.nAvgBytesPerSec=44100*format.nBlockAlign;
    Memory io;CAPECompressCore encoder(&io,&format,size/format.nBlockAlign,level);
    if(encoder.EncodeFrame(pcm.data(),size)) return 4;
    if(encoder.GetBitArray()->OutputBitArray(TRUE)) return 5;
    FILE* output=std::fopen(argv[2],"wb");if(!output) return 6;
    std::fwrite(io.bytes.data(),1,io.bytes.size(),output);return std::fclose(output);
}
