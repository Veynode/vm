// Basic ABI exercise against official libraries, not exhaustive ABI certification.
#include <stdio.h>
#include <string.h>
#include <glib.h>
#include <pixman.h>
#include <jpeglib.h>
#include <openssl/crypto.h>
#include <spice.h>
int main(void) {
 SpiceServer *server=spice_server_new();if(!server)return 3;spice_server_destroy(server);
 struct jpeg_decompress_struct jpeg; struct jpeg_error_mgr errors;
 jpeg.err=jpeg_std_error(&errors);jpeg_create_decompress(&jpeg);jpeg_destroy_decompress(&jpeg);
 printf("{\"glib_headers\":\"%d.%d.%d\",\"glib_runtime\":\"%u.%u.%u\",\"pixman_headers\":\"%s\",\"pixman_runtime\":\"%s\",\"openssl_header\":%lu,\"openssl_runtime\":%lu,\"spice_server_create_destroy\":true,\"jpeg_struct_create_destroy\":true}\n",GLIB_MAJOR_VERSION,GLIB_MINOR_VERSION,GLIB_MICRO_VERSION,glib_major_version,glib_minor_version,glib_micro_version,PIXMAN_VERSION_STRING,pixman_version_string(),(unsigned long)OPENSSL_VERSION_NUMBER,OpenSSL_version_num());
 return glib_major_version!=GLIB_MAJOR_VERSION || glib_minor_version!=GLIB_MINOR_VERSION || glib_micro_version!=GLIB_MICRO_VERSION || strcmp(PIXMAN_VERSION_STRING,pixman_version_string())!=0 || OpenSSL_version_num()!=OPENSSL_VERSION_NUMBER;
}
