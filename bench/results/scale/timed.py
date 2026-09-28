import os, sys, time, subprocess
t=time.time()
p=subprocess.Popen(sys.argv[1:], stdout=subprocess.DEVNULL)
_, status, ru = os.wait4(p.pid, 0)
dt=time.time()-t
print(f"{dt:7.2f} s   peak RSS {ru.ru_maxrss/1024:8.0f} MB   exit {os.waitstatus_to_exitcode(status)}")
