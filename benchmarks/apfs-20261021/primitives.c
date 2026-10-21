#include <sys/attr.h>
#include <sys/vnode.h>
#include <sys/stat.h>
#include <sys/snapshot.h>
#include <sys/xattr.h>
#include <sys/clonefile.h>
#include <dirent.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <time.h>
#include <CoreServices/CoreServices.h>
static long count;
static void fail(const char *s){perror(s);exit(1);}
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
static void scan(const char *path,int bulk){
 int fd=open(path,O_RDONLY|O_DIRECTORY);if(fd<0)fail(path);
 if(bulk){
  struct attrlist a={0};a.bitmapcount=ATTR_BIT_MAP_COUNT;
  a.commonattr=ATTR_CMN_RETURNED_ATTRS|ATTR_CMN_NAME|ATTR_CMN_OBJTYPE|ATTR_CMN_OBJID|ATTR_CMN_CRTIME|ATTR_CMN_MODTIME|ATTR_CMN_CHGTIME|ATTR_CMN_OWNERID|ATTR_CMN_GRPID|ATTR_CMN_ACCESSMASK;
  a.fileattr=ATTR_FILE_DATALENGTH;
  char buf[65536] __attribute__((aligned(8)));int n;
  while((n=getattrlistbulk(fd,&a,buf,sizeof(buf),0))>0){char *p=buf;for(int i=0;i<n;i++){
   uint32_t len;memcpy(&len,p,4);attribute_set_t returned;memcpy(&returned,p+4,sizeof(returned));
   if(!(returned.commonattr&ATTR_CMN_NAME)||!(returned.commonattr&ATTR_CMN_OBJTYPE))fail("missing attrs");
   attrreference_t name;memcpy(&name,p+4+sizeof(returned),sizeof(name));
   char *s=p+4+sizeof(returned)+name.attr_dataoffset;fsobj_type_t type;memcpy(&type,p+4+sizeof(returned)+sizeof(name),sizeof(type));
   char child[4096];snprintf(child,sizeof(child),"%s/%s",path,s);count++;
   if(type==VDIR)scan(child,bulk);p+=len;
  }}if(n<0)fail("getattrlistbulk");close(fd);
 }else{
  DIR *d=fdopendir(fd);if(!d)fail("fdopendir");struct dirent *e;
  while((e=readdir(d))){if(!strcmp(e->d_name,".")||!strcmp(e->d_name,".."))continue;
   char child[4096];snprintf(child,sizeof(child),"%s/%s",path,e->d_name);struct stat st;if(lstat(child,&st))fail("lstat");count++;if(S_ISDIR(st.st_mode))scan(child,0);
  }closedir(d);
 }
}
static void mutate(const char *path,const char *op){
 struct stat st;if(lstat(path,&st))fail("lstat");
 if(!strcmp(op,"unseal")&&chflags(path,0))fail("unseal");
 if(S_ISDIR(st.st_mode)){DIR*d=opendir(path);if(!d)fail("opendir");struct dirent*e;while((e=readdir(d))){if(!strcmp(e->d_name,".")||!strcmp(e->d_name,".."))continue;char p[4096];snprintf(p,sizeof(p),"%s/%s",path,e->d_name);mutate(p,op);}closedir(d);}
 if(!strcmp(op,"seal")&&chflags(path,UF_IMMUTABLE))fail("seal");
 if(!strcmp(op,"xattr")){char id[100];snprintf(id,sizeof(id),"%llu:%llu",(unsigned long long)st.st_dev,(unsigned long long)st.st_ino);if(setxattr(path,"com.layr.bench",id,strlen(id),0,XATTR_NOFOLLOW))fail("xattr");}
 count++;
}
static double event_start;static int found;static const char *expected;
static void events(ConstFSEventStreamRef stream,void*ctx,size_t n,void*paths,const FSEventStreamEventFlags flags[],const FSEventStreamEventId ids[]){
 char **p=paths;for(size_t i=0;i<n;i++){if(strstr(p[i],expected)){found++;printf("{\"operation\":\"event\",\"latency\":%.9f,\"flags\":%u}\n",now()-event_start,flags[i]);}}
}
int main(int argc,char**argv){if(argc<3)return 2;double start=now();char*op=argv[1];
 if(!strcmp(op,"stat"))scan(argv[2],0);
 else if(!strcmp(op,"bulk"))scan(argv[2],1);
 else if(!strcmp(op,"clone")){if(argc!=4)return 2;if(clonefile(argv[2],argv[3],CLONE_NOFOLLOW))fail("clone");}
 else if(!strcmp(op,"snapshot")){int fd=open(argv[2],O_RDONLY);int rc=fs_snapshot_create(fd,"layr-benchmark",0);int err=errno;printf("{\"operation\":\"snapshot_probe\",\"rc\":%d,\"errno\":%d}\n",rc,rc?err:0);if(!rc&&fs_snapshot_delete(fd,"layr-benchmark",0))fail("snapshot delete");close(fd);}
 else if(!strcmp(op,"events")){
  expected="event-target";CFStringRef s=CFStringCreateWithCString(NULL,argv[2],kCFStringEncodingUTF8);CFArrayRef a=CFArrayCreate(NULL,(const void**)&s,1,&kCFTypeArrayCallBacks);
  FSEventStreamRef stream=FSEventStreamCreate(NULL,events,NULL,a,kFSEventStreamEventIdSinceNow,0.05,kFSEventStreamCreateFlagFileEvents|kFSEventStreamCreateFlagNoDefer);
  FSEventStreamScheduleWithRunLoop(stream,CFRunLoopGetCurrent(),kCFRunLoopDefaultMode);if(!FSEventStreamStart(stream))fail("FSEventStreamStart");
  char p[4096];snprintf(p,sizeof(p),"%s/%s",argv[2],expected);event_start=now();int fd=open(p,O_CREAT|O_TRUNC|O_WRONLY,0600);if(fd<0)fail("event open");if(write(fd,"changed",7)!=7)fail("event write");close(fd);
  while(!found&&now()-event_start<5)CFRunLoopRunInMode(kCFRunLoopDefaultMode,0.1,0);
  FSEventStreamStop(stream);FSEventStreamInvalidate(stream);FSEventStreamRelease(stream);CFRelease(a);CFRelease(s);if(!found)return 3;
 }else mutate(argv[2],op);
 printf("{\"operation\":\"%s\",\"wall\":%.9f,\"entries\":%ld}\n",op,now()-start,count);return 0;
}
