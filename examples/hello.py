def greet(name):
    return "Hello, " + name + "!"

for i in range(3):
    print(greet("world " + str(i)))

nums = [x*x for x in range(5)]
print("squares:", nums, "sum:", sum(nums))
