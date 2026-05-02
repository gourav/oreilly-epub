import json


with open('raw-cookies.txt', 'r') as f:
  content = f.read()

cookies = {}
for line in content.splitlines():
  parts = line.split()

  cookie_name, cookie_value = parts[0], parts[1]
  cookies[cookie_name] = cookie_value 

with open('cookies.json', 'w') as f:
  json.dump(cookies, f, indent=2)