%define MIN(A,B) ((A) - ((A) - (B)) * ((A) > (B)))
%define MAX(A,B) ((A) + ((B) - (A)) * ((A) < (B)))
%define RGB(R,G,B) (((R) * 65536) + ((G) * 256) + (B))
%define RGBA(R,G,B,A) (((R) * 65536) + ((G) * 256) + (B) + ((A) * 16777216))
%define HEX(VALUE) (("0x"&(VALUE))+0)
%define BIN(VALUE) (("0b"&(VALUE))+0)
%define POW(BASE,EXP) antiln(ln(BASE)*(EXP))
%define GAMMA(VALUE) antiln(ln(VALUE)/2.2)
%define POSITIVE_CLAMP(VALUE) (((VALUE)>0)*(VALUE))
%define NEGATIVE_CLAMP(VALUE) (((VALUE)<0)*(VALUE))
%define CLAMP(VALUE,MIN,MAX) (((VALUE)>(MIN))*((MAX)+((VALUE)-(MAX))*((VALUE)<(MAX))))
%define ACOSH(X) ln((X)+sqrt((X)*(X)-1))
%define ASINH(X) ln((X)+sqrt((X)*(X)+1))
%define ATANH(X) ln((1+(X))/(1-(X)))/2
%define COSH(X) ((antiln(X)+antiln(-(X)))/2)
%define SINH(X) ((antiln(X)-antiln(-(X)))/2)
%define TANH(X) ((antiln(X)-antiln(-(X)))/(antiln(X)+antiln(-(X))))
%define PI 3.141592653589793
%define E 2.718281828459045
